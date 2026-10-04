//! A three-valued evaluator for GitHub Actions `${{ }}` expressions.
//!
//! The audit asks "does this job / step / concurrency stanza behave correctly
//! when the event is `merge_group`?" without running anything, so contexts it
//! cannot know statically (`matrix`, `steps`, `secrets`, a `needs` output of a
//! job that actually ran, …) evaluate to [`Value::Unknown`]. Unknown propagates
//! through operators, and the audit treats any condition that does not come
//! out definitely truthy as **not covered** — a suite the audit cannot prove
//! runs is never reported green (`ci-principles.md` rule 6).
//!
//! Semantics follow GitHub's documented expression rules: `&&` / `||` return
//! an operand (not a boolean), `==` compares strings case-insensitively and
//! coerces mismatched types to numbers, and the falsy values are `false`, `0`,
//! `-0`, `""` and `null`.

/// A runtime value, or what is statically known about it.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    /// Nothing is known.
    Unknown,
    /// The value is unknown, but its truthiness is known.
    Opaque(bool),
}

/// Three-valued truthiness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Truth {
    False,
    Unknown,
    True,
}

impl Value {
    #[must_use]
    pub fn truth(&self) -> Truth {
        match self {
            Value::Null => Truth::False,
            Value::Bool(b) | Value::Opaque(b) => {
                if *b {
                    Truth::True
                } else {
                    Truth::False
                }
            }
            Value::Num(n) => {
                if *n == 0.0 || n.is_nan() {
                    Truth::False
                } else {
                    Truth::True
                }
            }
            Value::Str(s) => {
                if s.is_empty() {
                    Truth::False
                } else {
                    Truth::True
                }
            }
            Value::Unknown => Truth::Unknown,
        }
    }

    fn known(&self) -> bool {
        !matches!(self, Value::Unknown | Value::Opaque(_))
    }

    /// GitHub's string conversion, for `format()` and interpolation.
    #[must_use]
    pub fn render(&self) -> Option<String> {
        Some(match self {
            Value::Null => String::new(),
            Value::Bool(b) => b.to_string(),
            Value::Num(n) => {
                if n.fract() == 0.0 && n.abs() < 1e15 {
                    format!("{n:.0}")
                } else {
                    n.to_string()
                }
            }
            Value::Str(s) => s.clone(),
            Value::Unknown | Value::Opaque(_) => return None,
        })
    }

    fn as_num(&self) -> f64 {
        match self {
            Value::Null => 0.0,
            Value::Bool(b) => f64::from(u8::from(*b)),
            Value::Num(n) => *n,
            Value::Str(s) => {
                let t = s.trim();
                if t.is_empty() {
                    0.0
                } else if let Some(hex) = t.strip_prefix("0x") {
                    i64::from_str_radix(hex, 16).map_or(f64::NAN, |v| v as f64)
                } else {
                    t.parse::<f64>().unwrap_or(f64::NAN)
                }
            }
            Value::Unknown | Value::Opaque(_) => f64::NAN,
        }
    }
}

/// Resolves context paths (`github.event_name`, `needs.x.outputs.y`, …) and
/// the status functions, whose meaning depends on where the expression sits.
pub trait Context {
    /// The value at a dotted context path. Unknown contexts answer `Unknown`.
    fn lookup(&self, path: &[String]) -> Value;
    /// `success()` at this position.
    fn success(&self) -> Value {
        Value::Bool(true)
    }
}

/// A parsed expression.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Lit(Value),
    /// A context path. A segment of `None` is an index the parser could not
    /// resolve to a literal (`matrix[foo]`), which looks up as `Unknown`.
    Path(Vec<Option<String>>),
    Call(String, Vec<Expr>),
    Not(Box<Expr>),
    Bin(Op, Box<Expr>, Box<Expr>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    And,
    Or,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl Expr {
    /// Whether the expression calls a status-check function anywhere. GitHub
    /// wraps a job/step `if:` without one in `success() && (...)`.
    #[must_use]
    pub fn has_status_function(&self) -> bool {
        match self {
            Expr::Call(name, args) => {
                matches!(
                    name.to_ascii_lowercase().as_str(),
                    "always" | "success" | "failure" | "cancelled"
                ) || args.iter().any(Expr::has_status_function)
            }
            Expr::Not(e) => e.has_status_function(),
            Expr::Bin(_, a, b) => a.has_status_function() || b.has_status_function(),
            Expr::Lit(_) | Expr::Path(_) => false,
        }
    }

    /// Every resolved context path the expression reads, dotted.
    pub fn paths(&self, out: &mut Vec<String>) {
        match self {
            Expr::Path(segs) => out.push(
                segs.iter()
                    .map(|s| s.as_deref().unwrap_or("*"))
                    .collect::<Vec<_>>()
                    .join("."),
            ),
            Expr::Call(_, args) => args.iter().for_each(|a| a.paths(out)),
            Expr::Not(e) => e.paths(out),
            Expr::Bin(_, a, b) => {
                a.paths(out);
                b.paths(out);
            }
            Expr::Lit(_) => {}
        }
    }

    #[must_use]
    pub fn eval(&self, ctx: &dyn Context) -> Value {
        match self {
            Expr::Lit(v) => v.clone(),
            Expr::Path(segs) => {
                let resolved: Option<Vec<String>> = segs.iter().cloned().collect();
                match resolved {
                    Some(p) => ctx.lookup(&p),
                    None => Value::Unknown,
                }
            }
            Expr::Not(e) => match e.eval(ctx).truth() {
                Truth::True => Value::Bool(false),
                Truth::False => Value::Bool(true),
                Truth::Unknown => Value::Unknown,
            },
            Expr::Bin(Op::And, a, b) => {
                let l = a.eval(ctx);
                match l.truth() {
                    Truth::False => l,
                    Truth::True => b.eval(ctx),
                    Truth::Unknown => match b.eval(ctx).truth() {
                        Truth::False => Value::Opaque(false),
                        _ => Value::Unknown,
                    },
                }
            }
            Expr::Bin(Op::Or, a, b) => {
                let l = a.eval(ctx);
                match l.truth() {
                    Truth::True => l,
                    Truth::False => b.eval(ctx),
                    Truth::Unknown => match b.eval(ctx).truth() {
                        Truth::True => Value::Opaque(true),
                        _ => Value::Unknown,
                    },
                }
            }
            Expr::Bin(op, a, b) => compare(*op, &a.eval(ctx), &b.eval(ctx)),
            Expr::Call(name, args) => call(name, args, ctx),
        }
    }
}

fn compare(op: Op, l: &Value, r: &Value) -> Value {
    if !l.known() || !r.known() {
        return Value::Unknown;
    }
    let ord = match (l, r) {
        (Value::Str(a), Value::Str(b)) => Some(a.to_lowercase().cmp(&b.to_lowercase())),
        (Value::Null, Value::Null) => Some(std::cmp::Ordering::Equal),
        (Value::Bool(a), Value::Bool(b)) => Some(a.cmp(b)),
        _ => l.as_num().partial_cmp(&r.as_num()),
    };
    let result = match (op, ord) {
        (Op::Eq, Some(o)) => o.is_eq(),
        (Op::Ne, Some(o)) => !o.is_eq(),
        (Op::Ne, None) => true,
        (Op::Lt, Some(o)) => o.is_lt(),
        (Op::Le, Some(o)) => o.is_le(),
        (Op::Gt, Some(o)) => o.is_gt(),
        (Op::Ge, Some(o)) => o.is_ge(),
        _ => false,
    };
    Value::Bool(result)
}

fn call(name: &str, args: &[Expr], ctx: &dyn Context) -> Value {
    let vals = || args.iter().map(|a| a.eval(ctx)).collect::<Vec<_>>();
    match name.to_ascii_lowercase().as_str() {
        "always" => Value::Bool(true),
        "success" => ctx.success(),
        // The audit models the run in which nothing upstream failed and
        // nobody cancelled: those are the runs a green merge group comes from.
        "failure" | "cancelled" => Value::Bool(false),
        "contains" | "startswith" | "endswith" => {
            let v = vals();
            let (Some(Some(hay)), Some(Some(needle))) =
                (v.first().map(Value::render), v.get(1).map(Value::render))
            else {
                return Value::Unknown;
            };
            let (h, n) = (hay.to_lowercase(), needle.to_lowercase());
            Value::Bool(match name.to_ascii_lowercase().as_str() {
                "contains" => h.contains(&n),
                "startswith" => h.starts_with(&n),
                _ => h.ends_with(&n),
            })
        }
        "format" => {
            let v = vals();
            let Some(Some(fmt)) = v.first().map(Value::render) else {
                return Value::Unknown;
            };
            let mut out = fmt;
            for (i, arg) in v.iter().enumerate().skip(1) {
                let Some(s) = arg.render() else {
                    return Value::Unknown;
                };
                out = out.replace(&format!("{{{}}}", i - 1), &s);
            }
            Value::Str(out)
        }
        _ => Value::Unknown,
    }
}

// --- parsing -----------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    LParen,
    RParen,
    LBracket,
    RBracket,
    Comma,
    Dot,
    Star,
    Not,
    Op(Op),
    Str(String),
    Num(f64),
    Ident(String),
}

fn lex(src: &str) -> Result<Vec<Tok>, String> {
    let chars: Vec<char> = src.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        match c {
            ' ' | '\t' | '\n' | '\r' => i += 1,
            '(' => {
                out.push(Tok::LParen);
                i += 1;
            }
            ')' => {
                out.push(Tok::RParen);
                i += 1;
            }
            '[' => {
                out.push(Tok::LBracket);
                i += 1;
            }
            ']' => {
                out.push(Tok::RBracket);
                i += 1;
            }
            ',' => {
                out.push(Tok::Comma);
                i += 1;
            }
            '.' => {
                out.push(Tok::Dot);
                i += 1;
            }
            '*' => {
                out.push(Tok::Star);
                i += 1;
            }
            '!' if next == Some('=') => {
                out.push(Tok::Op(Op::Ne));
                i += 2;
            }
            '!' => {
                out.push(Tok::Not);
                i += 1;
            }
            '=' if next == Some('=') => {
                out.push(Tok::Op(Op::Eq));
                i += 2;
            }
            '&' if next == Some('&') => {
                out.push(Tok::Op(Op::And));
                i += 2;
            }
            '|' if next == Some('|') => {
                out.push(Tok::Op(Op::Or));
                i += 2;
            }
            '<' | '>' => {
                let eq = next == Some('=');
                out.push(Tok::Op(match (c, eq) {
                    ('<', false) => Op::Lt,
                    ('<', true) => Op::Le,
                    ('>', false) => Op::Gt,
                    _ => Op::Ge,
                }));
                i += if eq { 2 } else { 1 };
            }
            '\'' => {
                let mut s = String::new();
                i += 1;
                loop {
                    match chars.get(i) {
                        None => return Err("unterminated string literal".to_string()),
                        Some('\'') if chars.get(i + 1) == Some(&'\'') => {
                            s.push('\'');
                            i += 2;
                        }
                        Some('\'') => {
                            i += 1;
                            break;
                        }
                        Some(ch) => {
                            s.push(*ch);
                            i += 1;
                        }
                    }
                }
                out.push(Tok::Str(s));
            }
            c if c.is_ascii_digit() || (c == '-' && next.is_some_and(|n| n.is_ascii_digit())) => {
                let start = i;
                i += 1;
                while chars
                    .get(i)
                    .is_some_and(|ch| ch.is_ascii_alphanumeric() || *ch == '.')
                {
                    i += 1;
                }
                let text: String = chars[start..i].iter().collect();
                let v = Value::Str(text.clone()).as_num();
                if v.is_nan() {
                    return Err(format!("bad number literal `{text}`"));
                }
                out.push(Tok::Num(v));
            }
            c if c.is_ascii_alphabetic() || c == '_' => {
                let start = i;
                while chars
                    .get(i)
                    .is_some_and(|ch| ch.is_ascii_alphanumeric() || *ch == '_' || *ch == '-')
                {
                    i += 1;
                }
                out.push(Tok::Ident(chars[start..i].iter().collect()));
            }
            other => return Err(format!("unexpected character `{other}`")),
        }
    }
    Ok(out)
}

struct P {
    toks: Vec<Tok>,
    pos: usize,
}

impl P {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }

    fn bump(&mut self) -> Option<Tok> {
        let t = self.toks.get(self.pos).cloned();
        self.pos += 1;
        t
    }

    fn expect(&mut self, t: &Tok) -> Result<(), String> {
        match self.bump() {
            Some(ref got) if got == t => Ok(()),
            other => Err(format!("expected {t:?}, found {other:?}")),
        }
    }

    fn expr(&mut self, min_prec: u8) -> Result<Expr, String> {
        let mut lhs = self.unary()?;
        while let Some(Tok::Op(op)) = self.peek().cloned() {
            let prec = match op {
                Op::Or => 1,
                Op::And => 2,
                Op::Eq | Op::Ne => 3,
                Op::Lt | Op::Le | Op::Gt | Op::Ge => 4,
            };
            if prec < min_prec {
                break;
            }
            self.bump();
            let rhs = self.expr(prec + 1)?;
            lhs = Expr::Bin(op, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn unary(&mut self) -> Result<Expr, String> {
        if self.peek() == Some(&Tok::Not) {
            self.bump();
            return Ok(Expr::Not(Box::new(self.unary()?)));
        }
        self.primary()
    }

    fn primary(&mut self) -> Result<Expr, String> {
        match self.bump() {
            Some(Tok::LParen) => {
                let e = self.expr(0)?;
                self.expect(&Tok::RParen)?;
                Ok(e)
            }
            Some(Tok::Str(s)) => Ok(Expr::Lit(Value::Str(s))),
            Some(Tok::Num(n)) => Ok(Expr::Lit(Value::Num(n))),
            Some(Tok::Ident(id)) => match id.as_str() {
                "true" => Ok(Expr::Lit(Value::Bool(true))),
                "false" => Ok(Expr::Lit(Value::Bool(false))),
                "null" => Ok(Expr::Lit(Value::Null)),
                _ if self.peek() == Some(&Tok::LParen) => {
                    self.bump();
                    let mut args = Vec::new();
                    if self.peek() != Some(&Tok::RParen) {
                        loop {
                            args.push(self.expr(0)?);
                            if self.peek() == Some(&Tok::Comma) {
                                self.bump();
                                continue;
                            }
                            break;
                        }
                    }
                    self.expect(&Tok::RParen)?;
                    Ok(Expr::Call(id, args))
                }
                _ => {
                    let mut segs = vec![Some(id)];
                    loop {
                        match self.peek() {
                            Some(Tok::Dot) => {
                                self.bump();
                                match self.bump() {
                                    Some(Tok::Ident(s)) => segs.push(Some(s)),
                                    Some(Tok::Star) => segs.push(None),
                                    other => {
                                        return Err(format!(
                                            "expected a property name, found {other:?}"
                                        ))
                                    }
                                }
                            }
                            Some(Tok::LBracket) => {
                                self.bump();
                                let idx = self.expr(0)?;
                                self.expect(&Tok::RBracket)?;
                                segs.push(match idx {
                                    Expr::Lit(Value::Str(s)) => Some(s),
                                    _ => None,
                                });
                            }
                            _ => break,
                        }
                    }
                    Ok(Expr::Path(segs))
                }
            },
            other => Err(format!("unexpected token {other:?}")),
        }
    }
}

/// Parse a bare expression (no `${{ }}` wrapper).
///
/// # Errors
///
/// On any syntax the subset does not understand.
pub fn parse(src: &str) -> Result<Expr, String> {
    let mut p = P {
        toks: lex(src)?,
        pos: 0,
    };
    let e = p.expr(0)?;
    if p.pos != p.toks.len() {
        return Err(format!("trailing tokens after position {}", p.pos));
    }
    Ok(e)
}

/// Parse an `if:` condition, which may or may not be wrapped in `${{ }}`.
///
/// # Errors
///
/// As [`parse`].
pub fn parse_condition(src: &str) -> Result<Expr, String> {
    let t = src.trim();
    match t.strip_prefix("${{").and_then(|r| r.strip_suffix("}}")) {
        Some(inner) if !inner.contains("${{") => parse(inner),
        _ => parse(t),
    }
}

/// Evaluate a string that may embed `${{ }}` interpolations. A string that is
/// exactly one interpolation yields that expression's value unconverted.
#[must_use]
pub fn interpolate(src: &str, ctx: &dyn Context) -> Value {
    let t = src.trim();
    if let Some(inner) = t.strip_prefix("${{").and_then(|r| r.strip_suffix("}}")) {
        if !inner.contains("${{") && !inner.contains("}}") {
            return parse(inner).map_or(Value::Unknown, |e| e.eval(ctx));
        }
    }
    let mut out = String::new();
    let mut rest = src;
    while let Some(start) = rest.find("${{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 3..];
        let Some(end) = after.find("}}") else {
            return Value::Unknown;
        };
        let v = parse(&after[..end]).map_or(Value::Unknown, |e| e.eval(ctx));
        match v.render() {
            Some(s) => out.push_str(&s),
            None => return Value::Unknown,
        }
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    Value::Str(out)
}

/// The context paths a (possibly interpolated) string reads.
#[must_use]
pub fn interpolated_paths(src: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = src;
    while let Some(start) = rest.find("${{") {
        let after = &rest[start + 3..];
        let Some(end) = after.find("}}") else { break };
        if let Ok(e) = parse(&after[..end]) {
            e.paths(&mut out);
        }
        rest = &after[end + 2..];
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    struct Ctx;
    impl Context for Ctx {
        fn lookup(&self, path: &[String]) -> Value {
            match path.join(".").as_str() {
                "github.event_name" => Value::Str("merge_group".into()),
                "github.ref" => Value::Str("refs/heads/gh-readonly-queue/main/pr-1-abc".into()),
                "github.event.pull_request.number" => Value::Null,
                "needs.changes.outputs.backend" => Value::Str(String::new()),
                _ => Value::Unknown,
            }
        }
    }

    fn ev(s: &str) -> Value {
        parse_condition(s).unwrap().eval(&Ctx)
    }

    #[test]
    fn logical_operators_return_operands() {
        assert_eq!(
            ev("github.event.pull_request.number || github.ref"),
            Value::Str("refs/heads/gh-readonly-queue/main/pr-1-abc".into())
        );
        assert_eq!(ev("github.event_name == 'push' && 'x'"), Value::Bool(false));
    }

    #[test]
    fn equality_is_case_insensitive_and_coerces() {
        assert_eq!(ev("github.event_name == 'MERGE_GROUP'"), Value::Bool(true));
        assert_eq!(ev("needs.changes.outputs.backend != 'false'"), Value::Bool(true));
        assert_eq!(ev("null == ''"), Value::Bool(true));
        assert_eq!(ev("1 == '1'"), Value::Bool(true));
    }

    #[test]
    fn unknown_propagates_but_short_circuits() {
        assert_eq!(ev("matrix.os == 'x'"), Value::Unknown);
        assert_eq!(
            ev("matrix.os == 'x' || github.event_name == 'merge_group'").truth(),
            Truth::True
        );
        assert_eq!(ev("matrix.os == 'x' && github.event_name == 'push'").truth(), Truth::False);
        assert_eq!(ev("!matrix.os"), Value::Unknown);
    }

    #[test]
    fn functions_and_status_detection() {
        assert_eq!(
            ev("format('mg-{0}-{1}', github.event_name, 7)"),
            Value::Str("mg-merge_group-7".into())
        );
        assert_eq!(
            ev("startsWith(github.ref, 'refs/heads/gh-readonly-queue/')"),
            Value::Bool(true)
        );
        assert!(parse_condition("${{ !cancelled() && x }}")
            .unwrap()
            .has_status_function());
        assert!(!parse_condition("github.event_name == 'push'")
            .unwrap()
            .has_status_function());
    }

    #[test]
    fn interpolation_concatenates_and_fails_closed() {
        assert_eq!(
            interpolate("ci-${{ github.event_name }}-x", &Ctx),
            Value::Str("ci-merge_group-x".into())
        );
        assert_eq!(interpolate("ci-${{ matrix.k }}", &Ctx), Value::Unknown);
        assert_eq!(
            interpolate("${{ github.event_name == 'pull_request' }}", &Ctx),
            Value::Bool(false)
        );
        assert_eq!(
            interpolated_paths("a-${{ github.ref }}-${{ matrix.k }}"),
            vec!["github.ref", "matrix.k"]
        );
    }

    #[test]
    fn syntax_errors_are_errors() {
        assert!(parse("a ==").is_err());
        assert!(parse("'open").is_err());
        assert!(parse("a b").is_err());
    }
}
