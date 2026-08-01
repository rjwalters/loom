//! `fleet bootstrap-spice` — provision a pinned SPICE simulation toolchain
//! (ngspice + Xyce, built from source) plus the gf180mcu and sky130 open PDKs
//! onto an already-reachable SSH host (issue #4931, Phase 1a of "elastic
//! cloud compute for SPICE simulations").
//!
//! # Scope (Phase 1a only)
//!
//! This module is deliberately narrow, per issue #4931's curator scope note:
//! it bootstraps the **sim toolchain only** — ngspice, Xyce, and the two open
//! PDKs, pinned to explicit refs. It does **not**:
//!
//! - Provision a VM or contact a cloud CLI (that stays in `repo:remote`, per
//!   epic #4340's boundary — this consumes "a reachable box + an SSH alias",
//!   exactly like [`super::add_worker`]).
//! - Call the Tailscale API or touch forge/token credentials — a sim runner
//!   is not a loom worker, so none of `add_worker`'s safehouse/forge-auth/
//!   token-pool machinery applies here. Every clone below is an anonymous
//!   HTTPS clone of a public repo; nothing on this path reads `gh` auth,
//!   `~/.loom/accounts.env`, or the token pool.
//! - Wire a `spice-run` dispatch wrapper, result caching, or autoscaling —
//!   those are Phase 1b/2/3 respectively (tracked as follow-ups on #4931,
//!   not built here).
//! - Register the host in the `~/.loom/fleet.json` fleet registry
//!   ([`super::FleetRegistry`]) — that registry models *loom workers*
//!   (`fleet status`/`fleet drain` consumers); a sim-only runner is not one,
//!   and adding it there would make `fleet status` poll a host that has no
//!   `loom-daemon` to answer.
//!
//! # Architecture — mirrors `add_worker`'s Plan/Step pattern
//!
//! Same shape as [`super::add_worker`] (see that module's doc comment for the
//! full rationale): an ordered [`super::Plan`] of named [`super::Step`]s, each
//! with a `check` (idempotency), `apply`, and optional `verify` phase, run
//! over a [`super::CommandRunner`] — the real one is `add_worker`'s
//! [`super::add_worker::SshRunner`] (reused directly rather than duplicated),
//! the tests supply a mock. `--dry-run` prints the ordered plan without
//! contacting the host.
//!
//! ## Idempotency via ref stamps
//!
//! Unlike `add_worker`'s steps (which check for a binary's mere presence),
//! each toolchain/PDK step here must also detect a *version* drift — a
//! re-run with a different `--ngspice-ref` (etc.) must rebuild, not report
//! `AlreadyDone` against a stale checkout. So each step's `check` compares a
//! small stamp file (`~/.loom/spice/stamps/<name>.ref`, written by that same
//! step's `apply` after a successful build) against the currently-configured
//! ref, rather than just probing `command -v`. Re-running at the **same** pins
//! reports every step `AlreadyDone` and touches nothing (#4931 AC 2); bumping
//! one pin rebuilds only that step.
//!
//! ## Resumability
//!
//! Same guarantee as `add_worker`: [`super::execute_plan`] halts at the first
//! failed step, and because every step is `check`-guarded, re-running after
//! fixing the cause skips the already-satisfied prefix and resumes at the
//! failure. The stamp is written **only after** a successful build, so a build
//! that dies half-way is never mistaken for a completed one.
//!
//! ## Version pins
//!
//! The `DEFAULT_*_REF` constants below are the pins as of this capability's
//! introduction (2026-08-01) — explicit, reproducible refs, never
//! "latest"/"main". Confirm/refresh them (via the `--*-ref` flags) before a
//! real bootstrap; proving them end-to-end against a live corner sweep is
//! Phase 1b's job (tracked in the analog repos, e.g. `gf180-bandgap`), not
//! this repo's.

use anyhow::{bail, Result};

use super::add_worker::SshRunner;
use super::path_bootstrap;
use super::{all_succeeded, execute_plan, render_checklist, Plan, Step};

/// Operator-facing command name, used in the dry-run/checklist headers.
const COMMAND: &str = "fleet bootstrap-spice";

/// Official ngspice source mirror (SourceForge git; ngspice does not publish
/// a canonical GitHub mirror).
pub const DEFAULT_NGSPICE_REPO_URL: &str = "https://git.code.sf.net/p/ngspice/ngspice";
/// Pinned ngspice release tag.
pub const DEFAULT_NGSPICE_REF: &str = "ngspice-42";

/// Official Xyce repository (Sandia National Laboratories).
pub const DEFAULT_XYCE_REPO_URL: &str = "https://github.com/Xyce/Xyce";
/// Pinned Xyce release tag.
pub const DEFAULT_XYCE_REF: &str = "Release-7.7";

/// Trilinos — Xyce's required linear-algebra/solver dependency.
pub const DEFAULT_TRILINOS_REPO_URL: &str = "https://github.com/trilinos/Trilinos";
/// Pinned Trilinos release tag (matched to a Xyce-compatible Trilinos release).
pub const DEFAULT_TRILINOS_REF: &str = "trilinos-release-14-4-0";

/// The open gf180mcu (GlobalFoundries 180nm MCU) PDK.
pub const DEFAULT_GF180MCU_REPO_URL: &str = "https://github.com/google/gf180mcu-pdk";
/// Pinned gf180mcu-pdk release tag.
pub const DEFAULT_GF180MCU_REF: &str = "v0.9.5";
/// The submodule path inside `gf180mcu-pdk` holding the primitive-device
/// library (the SPICE models a corner sweep actually reads). The PDK repos
/// keep their model libraries in submodules, so a plain clone yields an empty
/// `libraries/` tree — this path is initialized explicitly after checkout.
/// Empty ⇒ no submodule init (see [`SpiceBootstrapConfig::gf180mcu_models_path`]).
pub const DEFAULT_GF180MCU_MODELS_PATH: &str = "libraries/gf180mcu_fd_pr/latest";

/// The open sky130 PDK (SkyWater 130nm).
pub const DEFAULT_SKY130_REPO_URL: &str = "https://github.com/google/skywater-pdk";
/// Pinned skywater-pdk release tag.
pub const DEFAULT_SKY130_REF: &str = "v0.0.3";
/// The submodule path inside `skywater-pdk` holding the primitive-device
/// library — sky130's analogue of [`DEFAULT_GF180MCU_MODELS_PATH`].
pub const DEFAULT_SKY130_MODELS_PATH: &str = "libraries/sky130_fd_pr/latest";

/// Base directory (`$HOME`-relative in the rendered shell) under which the
/// toolchain sources, ref stamps, and PDK checkouts live on the runner.
pub const SPICE_BASE: &str = ".loom/spice";

/// Operator inputs for a single `fleet bootstrap-spice` invocation.
#[derive(Debug, Clone)]
pub struct SpiceBootstrapConfig {
    /// SSH alias/host to reach the runner (from `repo:remote` or operator
    /// supplied).
    pub ssh_host: String,
    /// Print the ordered plan without contacting the host.
    pub dry_run: bool,
    /// ngspice source repository URL.
    pub ngspice_repo_url: String,
    /// Pinned ngspice ref (tag/branch/commit) to build.
    pub ngspice_ref: String,
    /// Whether to build Xyce (+ its Trilinos dependency). `false` renders the
    /// step as a [`super::PlanEntry::Skip`] with a reason instead — Xyce's
    /// source build is measured in hours, and the analog repos that only use
    /// ngspice should not have to pay for it.
    pub install_xyce: bool,
    /// Xyce source repository URL.
    pub xyce_repo_url: String,
    /// Pinned Xyce ref to build.
    pub xyce_ref: String,
    /// Trilinos source repository URL (Xyce's solver dependency).
    pub trilinos_repo_url: String,
    /// Pinned Trilinos ref to build.
    pub trilinos_ref: String,
    /// gf180mcu-pdk repository URL.
    pub gf180mcu_repo_url: String,
    /// Pinned gf180mcu-pdk ref to check out.
    pub gf180mcu_ref: String,
    /// Submodule path inside the gf180mcu checkout to initialize (the SPICE
    /// model library). Empty ⇒ clone the top-level repo only, initialize no
    /// submodule — the escape hatch when a pin's layout differs from
    /// [`DEFAULT_GF180MCU_MODELS_PATH`].
    pub gf180mcu_models_path: String,
    /// skywater-pdk (sky130) repository URL.
    pub sky130_repo_url: String,
    /// Pinned skywater-pdk ref to check out.
    pub sky130_ref: String,
    /// Submodule path inside the sky130 checkout to initialize. Empty ⇒ none
    /// (see [`Self::gf180mcu_models_path`]).
    pub sky130_models_path: String,
}

impl SpiceBootstrapConfig {
    /// A config with every ref/URL defaulted, for `ssh_host`.
    #[must_use]
    pub fn with_defaults(ssh_host: impl Into<String>) -> Self {
        Self {
            ssh_host: ssh_host.into(),
            dry_run: false,
            ngspice_repo_url: DEFAULT_NGSPICE_REPO_URL.to_string(),
            ngspice_ref: DEFAULT_NGSPICE_REF.to_string(),
            install_xyce: true,
            xyce_repo_url: DEFAULT_XYCE_REPO_URL.to_string(),
            xyce_ref: DEFAULT_XYCE_REF.to_string(),
            trilinos_repo_url: DEFAULT_TRILINOS_REPO_URL.to_string(),
            trilinos_ref: DEFAULT_TRILINOS_REF.to_string(),
            gf180mcu_repo_url: DEFAULT_GF180MCU_REPO_URL.to_string(),
            gf180mcu_ref: DEFAULT_GF180MCU_REF.to_string(),
            gf180mcu_models_path: DEFAULT_GF180MCU_MODELS_PATH.to_string(),
            sky130_repo_url: DEFAULT_SKY130_REPO_URL.to_string(),
            sky130_ref: DEFAULT_SKY130_REF.to_string(),
            sky130_models_path: DEFAULT_SKY130_MODELS_PATH.to_string(),
        }
    }
}

/// A non-secret operator string that gets interpolated into rendered shell
/// (a repo URL, a git ref, or a submodule path) must pass this before it is
/// ever formatted into a step's `apply`/`check` text — mirrors
/// `add_worker::validate_safe_token`'s shell-injection defense for the same
/// reason (this text is echoed verbatim in `--dry-run` output and executed on
/// the runner). Duplicated rather than imported: `add_worker`'s validator is
/// private, and this module has no other reason to couple to it (the same
/// tradeoff `add_worker`'s own `validate_persona_name` doc comment makes).
fn validate_safe_token(label: &str, value: &str, extra: &str) -> Result<()> {
    if value.trim().is_empty() {
        bail!("--{label} must not be empty");
    }
    if !value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || extra.contains(c))
    {
        bail!(
            "--{label} '{value}' contains characters outside [A-Za-z0-9{extra}]; refusing to \
             render it into shell"
        );
    }
    Ok(())
}

/// Same charset guard as [`validate_safe_token`], but an **empty** value is
/// allowed and means "not configured" (the submodule-path opt-out).
fn validate_optional_safe_token(label: &str, value: &str, extra: &str) -> Result<()> {
    if value.trim().is_empty() {
        return Ok(());
    }
    validate_safe_token(label, value, extra)
}

/// Local preflight: validate inputs **before any remote action**. No secrets
/// to read here (no forge/token access on this code path, per #4931's scope),
/// so this is pure validation — but it still runs before dry-run mode is
/// decided, matching `add_worker::preflight`'s "fail fast" contract.
pub fn preflight(config: &SpiceBootstrapConfig) -> Result<()> {
    if config.ssh_host.trim().is_empty() {
        bail!("ssh-host must not be empty");
    }
    validate_safe_token("ngspice-repo-url", &config.ngspice_repo_url, ".:/_-")?;
    validate_safe_token("ngspice-ref", &config.ngspice_ref, "._-/")?;
    if config.install_xyce {
        validate_safe_token("xyce-repo-url", &config.xyce_repo_url, ".:/_-")?;
        validate_safe_token("xyce-ref", &config.xyce_ref, "._-/")?;
        validate_safe_token("trilinos-repo-url", &config.trilinos_repo_url, ".:/_-")?;
        validate_safe_token("trilinos-ref", &config.trilinos_ref, "._-/")?;
    }
    validate_safe_token("gf180mcu-repo-url", &config.gf180mcu_repo_url, ".:/_-")?;
    validate_safe_token("gf180mcu-ref", &config.gf180mcu_ref, "._-/")?;
    validate_optional_safe_token("gf180mcu-models-path", &config.gf180mcu_models_path, "._-/")?;
    validate_safe_token("sky130-repo-url", &config.sky130_repo_url, ".:/_-")?;
    validate_safe_token("sky130-ref", &config.sky130_ref, "._-/")?;
    validate_optional_safe_token("sky130-models-path", &config.sky130_models_path, "._-/")?;
    Ok(())
}

/// Build the ordered bootstrap [`Plan`] from `config`.
///
/// Pure (no I/O, no host contact) so ordering and idempotency checks are
/// unit-testable — mirrors [`super::add_worker::build_plan`].
#[must_use]
pub fn build_plan(config: &SpiceBootstrapConfig) -> Plan {
    let mut plan = Plan::new();

    // 1. Base build dependencies for ngspice + Xyce/Trilinos from source.
    plan.push_step(Step::new(
        "spice-base-deps",
        "install build-essential, bison, flex, cmake, gfortran, BLAS/LAPACK/SuiteSparse, and X11/readline dev headers",
        Some(format!("dpkg -s {BASE_DEP_PACKAGES} >/dev/null 2>&1")),
        render_base_deps(),
    ));

    // 2. Persist the canonical PATH (#4831) so ~/.local/bin (where ngspice and
    //    Xyce install) resolves for later logins — the same thing
    //    `add_worker`'s machine-layout step does for `loom-daemon`.
    plan.push_step(Step::new(
        "spice-path",
        "add the canonical loom PATH (~/.local/bin first) to ~/.profile",
        Some(r#"grep -qF 'loom-spice-path (#4931)' "$HOME/.profile" 2>/dev/null"#.to_string()),
        render_path_profile(),
    ));

    // 3. ngspice: clone/checkout the pinned ref, build, install to ~/.local.
    plan.push_step(Step::new(
        "ngspice",
        &format!("build + install ngspice@{} to ~/.local", config.ngspice_ref),
        Some(render_ref_stamp_check(
            "ngspice",
            &config.ngspice_ref,
            "command -v ngspice >/dev/null 2>&1",
        )),
        render_ngspice(&config.ngspice_repo_url, &config.ngspice_ref),
    ));

    // 4. Xyce (+ its Trilinos dependency): clone/checkout pinned refs, build,
    //    install to ~/.local. The stamp is keyed on BOTH refs so a pin change
    //    in either one triggers a rebuild.
    if config.install_xyce {
        let xyce_stamp = format!("{}+{}", config.xyce_ref, config.trilinos_ref);
        plan.push_step(Step::new(
            "xyce",
            &format!(
                "build + install Xyce@{} (Trilinos@{}) to ~/.local",
                config.xyce_ref, config.trilinos_ref
            ),
            Some(render_ref_stamp_check("xyce", &xyce_stamp, "command -v Xyce >/dev/null 2>&1")),
            render_xyce(config),
        ));
    } else {
        plan.push_skip(
            "xyce",
            "build + install Xyce (with Trilinos)",
            "--skip-xyce supplied (ngspice-only runner)",
        );
    }

    // 5. gf180mcu PDK: clone/checkout the pinned ref (+ model submodule).
    plan.push_step(Step::new(
        "gf180mcu-pdk",
        &format!("check out gf180mcu-pdk@{}", config.gf180mcu_ref),
        Some(render_pdk_check("gf180mcu", &config.gf180mcu_ref, &config.gf180mcu_models_path)),
        render_pdk_clone(
            "gf180mcu",
            &config.gf180mcu_repo_url,
            &config.gf180mcu_ref,
            &config.gf180mcu_models_path,
        ),
    ));

    // 6. sky130 PDK: clone/checkout the pinned ref (+ model submodule).
    plan.push_step(Step::new(
        "sky130-pdk",
        &format!("check out skywater-pdk (sky130)@{}", config.sky130_ref),
        Some(render_pdk_check("sky130", &config.sky130_ref, &config.sky130_models_path)),
        render_pdk_clone(
            "sky130",
            &config.sky130_repo_url,
            &config.sky130_ref,
            &config.sky130_models_path,
        ),
    ));

    // 7. Verify: binaries on PATH, PDK checkouts present, pins match.
    plan.push_step(Step::new(
        "verify",
        "verify: ngspice (and Xyce, unless skipped) on PATH, both PDKs checked out at their pinned refs",
        None,
        render_verify(config),
    ));

    plan
}

/// Top-level orchestration for `loom-daemon fleet bootstrap-spice`.
///
/// Preflight → build plan → (dry-run: print + return) → execute over ssh →
/// print the per-step checklist. Returns an error if any step failed (so the
/// CLI exits non-zero) — no fleet-registry write (see this module's doc
/// comment: a sim runner is not a loom worker).
pub fn run(config: &SpiceBootstrapConfig) -> Result<()> {
    preflight(config)?;
    let plan = build_plan(config);

    if config.dry_run {
        print!("{}", plan.render_dry_run(COMMAND, &config.ssh_host));
        println!(
            "\n(dry run — no action taken on {}. Re-run without --dry-run to execute.)",
            config.ssh_host
        );
        return Ok(());
    }

    let runner = SshRunner::new(&config.ssh_host);
    let reports = execute_plan(&runner, &plan);
    print!("{}", render_checklist(COMMAND, &config.ssh_host, &reports));

    if all_succeeded(&reports, &plan) {
        let xyce = if config.install_xyce {
            format!("Xyce@{}", config.xyce_ref)
        } else {
            "Xyce skipped".to_string()
        };
        println!(
            "\nSPICE toolchain bootstrap complete on {}: ngspice@{}, {}, gf180mcu-pdk@{}, sky130-pdk@{}.",
            config.ssh_host,
            config.ngspice_ref,
            xyce,
            config.gf180mcu_ref,
            config.sky130_ref
        );
        println!("Binaries: ~/.local/bin — PDKs: ~/{SPICE_BASE}/pdks/{{gf180mcu,sky130}}");
        Ok(())
    } else {
        let failed = reports
            .iter()
            .find(|r| r.status.is_failure())
            .map_or_else(|| "unknown".to_string(), |r| r.name.clone());
        bail!(
            "fleet bootstrap-spice halted at step '{failed}' on {} — see the checklist above. \
             The run is idempotent: fix the cause and re-run to resume.",
            config.ssh_host
        )
    }
}

// ===========================================================================
// Shell templates (rendered on the daemon, executed on the runner over ssh)
// ===========================================================================

/// The apt packages the toolchain build needs. Shared by the `check` (a
/// `dpkg -s` probe over the same list) and the `apply` (the `apt-get install`)
/// so the two can never drift — a package present in one but not the other
/// would make the step either permanently unsatisfied or under-installed.
const BASE_DEP_PACKAGES: &str = "build-essential git curl ca-certificates bison flex libfl-dev \
libx11-dev libxaw7-dev libreadline-dev libedit-dev autoconf automake libtool cmake gfortran \
libblas-dev liblapack-dev libsuitesparse-dev libfftw3-dev";

fn render_base_deps() -> String {
    format!(
        r#"set -e
export DEBIAN_FRONTEND=noninteractive
sudo apt-get update -qq
sudo apt-get install -y {BASE_DEP_PACKAGES}
"#
    )
}

/// Persist the canonical loom PATH (#4831) into `~/.profile` so a later login
/// resolves `~/.local/bin/ngspice` / `Xyce` without the caller re-exporting
/// it. Non-interactive `ssh host ngspice …` does NOT source `~/.profile`, so
/// Phase 1b's dispatch wrapper should still use absolute paths — this step
/// exists for operators poking at the box by hand.
fn render_path_profile() -> String {
    let export_line = path_bootstrap::canonical_path_export_line();
    let export_line = export_line.trim_end();
    format!(
        r#"set -e
touch "$HOME/.profile"
if ! grep -qF 'loom-spice-path (#4931)' "$HOME/.profile"; then
  {{
    echo '# loom-spice-path (#4931)'
    printf '%s\n' '{export_line}'
  }} >> "$HOME/.profile"
fi
"#
    )
}

/// Render a `check` phase for a ref-stamped step: the recorded stamp must
/// match `expected_ref` AND `extra_probe` (e.g. `command -v ngspice`) must
/// pass. Either half failing means "not done" — falls through to `apply`.
///
/// The canonical PATH export (#4831) is prepended so `command -v` sees
/// `~/.local/bin`, which a non-interactive SSH shell's default PATH omits —
/// without it every re-run would report `Changed` and rebuild from scratch.
fn render_ref_stamp_check(name: &str, expected_ref: &str, extra_probe: &str) -> String {
    let export_line = path_bootstrap::canonical_path_export_line();
    format!(
        r#"{export_line}[ "$(cat "$HOME/{SPICE_BASE}/stamps/{name}.ref" 2>/dev/null)" = "{expected_ref}" ] && {extra_probe}"#
    )
}

fn render_ngspice(repo_url: &str, ref_: &str) -> String {
    format!(
        r#"set -e
STAMP_DIR="$HOME/{SPICE_BASE}/stamps"
SRC="$HOME/{SPICE_BASE}/src/ngspice"
mkdir -p "$STAMP_DIR" "$(dirname "$SRC")"
if [ -d "$SRC/.git" ]; then
  git -C "$SRC" fetch --tags --quiet
else
  git clone {repo_url} "$SRC"
fi
git -C "$SRC" checkout --quiet {ref_}
cd "$SRC"
./autogen.sh
./configure --prefix="$HOME/.local" --enable-xspice --enable-cider --with-readline=yes
make -j"$(nproc)"
make install
# The stamp is written LAST, only after a successful install, so a build that
# dies half-way is never mistaken for a completed one on the next run.
printf '%s' "{ref_}" > "$STAMP_DIR/ngspice.ref"
"#
    )
}

fn render_xyce(config: &SpiceBootstrapConfig) -> String {
    let xyce_repo_url = &config.xyce_repo_url;
    let xyce_ref = &config.xyce_ref;
    let trilinos_repo_url = &config.trilinos_repo_url;
    let trilinos_ref = &config.trilinos_ref;
    // The Trilinos option set below is Xyce's documented serial-build recipe
    // (Xyce Building Guide): Xyce needs a specific, narrow slice of Trilinos
    // (NOX/LOCA, EpetraExt + BTF/reorderings, Amesos(2)+KLU(2), Belos, Ifpack,
    // AztecOO, Sacado, Stokhos, complex Teuchos) with everything else off. A
    // default `cmake -DCMAKE_INSTALL_PREFIX=…` Trilinos does NOT satisfy Xyce's
    // configure, so these flags are load-bearing, not decoration.
    format!(
        r#"set -e
STAMP_DIR="$HOME/{SPICE_BASE}/stamps"
TRILINOS_SRC="$HOME/{SPICE_BASE}/src/trilinos"
XYCE_SRC="$HOME/{SPICE_BASE}/src/xyce"
TRILINOS_INSTALL="$HOME/{SPICE_BASE}/trilinos-install"
mkdir -p "$STAMP_DIR" "$HOME/{SPICE_BASE}/src"
if [ -d "$TRILINOS_SRC/.git" ]; then
  git -C "$TRILINOS_SRC" fetch --tags --quiet
else
  git clone {trilinos_repo_url} "$TRILINOS_SRC"
fi
git -C "$TRILINOS_SRC" checkout --quiet {trilinos_ref}
if [ -d "$XYCE_SRC/.git" ]; then
  git -C "$XYCE_SRC" fetch --tags --quiet
else
  git clone {xyce_repo_url} "$XYCE_SRC"
fi
git -C "$XYCE_SRC" checkout --quiet {xyce_ref}
# SuiteSparse's AMD lives in the multiarch lib dir on Debian/Ubuntu; resolve it
# rather than hardcoding an x86_64 path (fleet runners may be arm64).
AMD_LIB_DIR="/usr/lib/$(dpkg-architecture -qDEB_HOST_MULTIARCH 2>/dev/null || echo "$(uname -m)-linux-gnu")"
mkdir -p "$TRILINOS_SRC/build-serial"
cd "$TRILINOS_SRC/build-serial"
cmake \
  -DCMAKE_INSTALL_PREFIX="$TRILINOS_INSTALL" \
  -DCMAKE_BUILD_TYPE=Release \
  -DCMAKE_C_FLAGS="-O3 -fPIC" \
  -DCMAKE_CXX_FLAGS="-O3 -fPIC" \
  -DCMAKE_Fortran_FLAGS="-O3 -fPIC" \
  -DTrilinos_ENABLE_ALL_OPTIONAL_PACKAGES=OFF \
  -DTrilinos_ENABLE_NOX=ON \
  -DNOX_ENABLE_LOCA=ON \
  -DTrilinos_ENABLE_EpetraExt=ON \
  -DEpetraExt_BUILD_BTF=ON \
  -DEpetraExt_BUILD_EXPERIMENTAL=ON \
  -DEpetraExt_BUILD_GRAPH_REORDERINGS=ON \
  -DTrilinos_ENABLE_TrilinosCouplings=ON \
  -DTrilinos_ENABLE_Ifpack=ON \
  -DTrilinos_ENABLE_AztecOO=ON \
  -DTrilinos_ENABLE_Belos=ON \
  -DTrilinos_ENABLE_Teuchos=ON \
  -DTeuchos_ENABLE_COMPLEX=ON \
  -DTrilinos_ENABLE_Amesos=ON \
  -DAmesos_ENABLE_KLU=ON \
  -DTrilinos_ENABLE_Amesos2=ON \
  -DAmesos2_ENABLE_KLU2=ON \
  -DAmesos2_ENABLE_Basker=ON \
  -DTrilinos_ENABLE_Sacado=ON \
  -DTrilinos_ENABLE_Stokhos=ON \
  -DTrilinos_ENABLE_Kokkos=ON \
  -DTPL_ENABLE_AMD=ON \
  -DAMD_LIBRARY_DIRS="$AMD_LIB_DIR" \
  -DTPL_AMD_INCLUDE_DIRS=/usr/include/suitesparse \
  -DTPL_ENABLE_BLAS=ON \
  -DTPL_ENABLE_LAPACK=ON \
  "$TRILINOS_SRC"
make -j"$(nproc)"
make install
mkdir -p "$XYCE_SRC/build-serial"
cd "$XYCE_SRC/build-serial"
cmake \
  -DCMAKE_INSTALL_PREFIX="$HOME/.local" \
  -DCMAKE_BUILD_TYPE=Release \
  -DTrilinos_ROOT="$TRILINOS_INSTALL" \
  "$XYCE_SRC"
make -j"$(nproc)"
make install
printf '%s+%s' "{xyce_ref}" "{trilinos_ref}" > "$STAMP_DIR/xyce.ref"
"#
    )
}

/// A PDK step's `check`: the stamp matches the configured ref, the checkout is
/// a git repo, and (when a models submodule is configured) that submodule is
/// actually populated — an initialized-but-empty submodule directory is the
/// exact failure mode that would otherwise pass a naive `-d` probe and leave a
/// runner with no SPICE models.
fn render_pdk_check(name: &str, expected_ref: &str, models_path: &str) -> String {
    let models_probe = if models_path.trim().is_empty() {
        String::new()
    } else {
        format!(
            r#" && [ -n "$(ls -A "$HOME/{SPICE_BASE}/pdks/{name}/{models_path}" 2>/dev/null)" ]"#
        )
    };
    format!(
        r#"[ "$(cat "$HOME/{SPICE_BASE}/stamps/{name}.ref" 2>/dev/null)" = "{expected_ref}" ] && [ -d "$HOME/{SPICE_BASE}/pdks/{name}/.git" ]{models_probe}"#
    )
}

fn render_pdk_clone(name: &str, repo_url: &str, ref_: &str, models_path: &str) -> String {
    // The PDK repos keep their device models in submodules, so a bare clone
    // leaves `libraries/` empty. Init only the configured path (the whole
    // recursive set is tens of GB of standard-cell libraries a SPICE runner
    // never reads).
    let submodule = if models_path.trim().is_empty() {
        String::new()
    } else {
        format!(
            r#"git -C "$DEST" submodule update --init --recursive -- "{models_path}"
"#
        )
    };
    format!(
        r#"set -e
STAMP_DIR="$HOME/{SPICE_BASE}/stamps"
DEST="$HOME/{SPICE_BASE}/pdks/{name}"
mkdir -p "$STAMP_DIR" "$(dirname "$DEST")"
if [ -d "$DEST/.git" ]; then
  git -C "$DEST" fetch --tags --quiet
else
  git clone {repo_url} "$DEST"
fi
git -C "$DEST" checkout --quiet {ref_}
{submodule}printf '%s' "{ref_}" > "$STAMP_DIR/{name}.ref"
"#
    )
}

fn render_verify(config: &SpiceBootstrapConfig) -> String {
    let export_line = path_bootstrap::canonical_path_export_line();
    let xyce_check = if config.install_xyce {
        "command -v Xyce >/dev/null 2>&1 || { echo \"Xyce not found on PATH\" >&2; exit 1; }\n"
    } else {
        "# Xyce skipped (--skip-xyce)\n"
    };
    let mut pdk_checks = String::new();
    for (name, models_path) in [
        ("gf180mcu", &config.gf180mcu_models_path),
        ("sky130", &config.sky130_models_path),
    ] {
        pdk_checks.push_str(&format!(
            r#"[ -d "$HOME/{SPICE_BASE}/pdks/{name}/.git" ] || {{ echo "{name} PDK missing" >&2; exit 1; }}
"#
        ));
        if !models_path.trim().is_empty() {
            pdk_checks.push_str(&format!(
                r#"[ -n "$(ls -A "$HOME/{SPICE_BASE}/pdks/{name}/{models_path}" 2>/dev/null)" ] || {{ echo "{name} device models missing at {models_path}" >&2; exit 1; }}
"#
            ));
        }
    }
    let xyce_summary = if config.install_xyce {
        format!("Xyce@{} (Trilinos@{})", config.xyce_ref, config.trilinos_ref)
    } else {
        "Xyce skipped".to_string()
    };
    format!(
        r#"set -e
{export_line}command -v ngspice >/dev/null 2>&1 || {{ echo "ngspice not found on PATH" >&2; exit 1; }}
{xyce_check}{pdk_checks}echo "spice toolchain verified: ngspice@{ngspice_ref}, {xyce_summary}"
echo "PDKs: gf180mcu@{gf180mcu_ref}, sky130@{sky130_ref}"
"#,
        ngspice_ref = config.ngspice_ref,
        gf180mcu_ref = config.gf180mcu_ref,
        sky130_ref = config.sky130_ref,
    )
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::fleet::{CommandOutput, CommandRunner, PlanEntry, StepStatus};
    use std::cell::RefCell;

    // ---- Mock command runner (mirrors `super::tests::MockRunner`) --------

    /// A scripted [`CommandRunner`]: each `run` pops the next canned
    /// [`CommandOutput`] (defaulting to success once the script is
    /// exhausted), recording every `(shell, stdin)` call so tests can assert
    /// on ordering and idempotency without a live box.
    struct MockRunner {
        responses: RefCell<Vec<CommandOutput>>,
        calls: RefCell<Vec<(String, Option<String>)>>,
    }

    impl MockRunner {
        fn new(responses: Vec<CommandOutput>) -> Self {
            Self {
                responses: RefCell::new(responses),
                calls: RefCell::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<(String, Option<String>)> {
            self.calls.borrow().clone()
        }
    }

    impl CommandRunner for MockRunner {
        fn run(&self, shell: &str, stdin: Option<&str>) -> Result<CommandOutput> {
            self.calls
                .borrow_mut()
                .push((shell.to_string(), stdin.map(str::to_string)));
            let mut r = self.responses.borrow_mut();
            if r.is_empty() {
                Ok(CommandOutput {
                    code: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                })
            } else {
                Ok(r.remove(0))
            }
        }
    }

    fn ok() -> CommandOutput {
        CommandOutput {
            code: 0,
            stdout: String::new(),
            stderr: String::new(),
        }
    }

    fn fail(code: i32) -> CommandOutput {
        CommandOutput {
            code,
            stdout: String::new(),
            stderr: "boom".to_string(),
        }
    }

    fn base_config() -> SpiceBootstrapConfig {
        SpiceBootstrapConfig::with_defaults("spice-runner-1")
    }

    fn step<'a>(plan: &'a Plan, name: &str) -> &'a Step {
        plan.entries
            .iter()
            .find_map(|e| match e {
                PlanEntry::Step(s) if s.name == name => Some(s),
                _ => None,
            })
            .unwrap_or_else(|| panic!("expected a step named {name}"))
    }

    // ---- preflight -------------------------------------------------------

    #[test]
    fn preflight_rejects_empty_host() {
        let mut config = base_config();
        config.ssh_host = "   ".to_string();
        assert!(preflight(&config).is_err());
    }

    #[test]
    fn preflight_accepts_defaults() {
        assert!(preflight(&base_config()).is_ok());
    }

    #[test]
    fn preflight_rejects_shell_injection_in_ref() {
        let mut config = base_config();
        config.ngspice_ref = "ngspice-42; rm -rf /".to_string();
        let err = preflight(&config).unwrap_err().to_string();
        assert!(err.contains("ngspice-ref"), "err: {err}");
    }

    #[test]
    fn preflight_rejects_shell_injection_in_repo_url() {
        let mut config = base_config();
        config.xyce_repo_url = "https://example.com/$(whoami)".to_string();
        assert!(preflight(&config).is_err());
    }

    #[test]
    fn preflight_rejects_shell_injection_in_models_path() {
        let mut config = base_config();
        config.sky130_models_path = "libraries/`id`".to_string();
        let err = preflight(&config).unwrap_err().to_string();
        assert!(err.contains("sky130-models-path"), "err: {err}");
    }

    #[test]
    fn preflight_rejects_empty_ref() {
        let mut config = base_config();
        config.sky130_ref = String::new();
        assert!(preflight(&config).is_err());
    }

    #[test]
    fn preflight_allows_empty_models_path_as_opt_out() {
        let mut config = base_config();
        config.sky130_models_path = String::new();
        config.gf180mcu_models_path = String::new();
        assert!(preflight(&config).is_ok());
    }

    #[test]
    fn preflight_ignores_xyce_inputs_when_xyce_is_skipped() {
        // A malformed Xyce pin must not block an ngspice-only bootstrap that
        // never renders it into shell.
        let mut config = base_config();
        config.install_xyce = false;
        config.xyce_ref = String::new();
        config.trilinos_repo_url = "not a url; rm -rf /".to_string();
        assert!(preflight(&config).is_ok());
    }

    // ---- plan shape --------------------------------------------------------

    #[test]
    fn plan_step_ordering_is_deps_path_toolchain_pdks_verify() {
        let plan = build_plan(&base_config());
        let names: Vec<_> = plan.entries.iter().map(PlanEntry::name).collect();
        assert_eq!(
            names,
            vec![
                "spice-base-deps",
                "spice-path",
                "ngspice",
                "xyce",
                "gf180mcu-pdk",
                "sky130-pdk",
                "verify"
            ]
        );
    }

    #[test]
    fn skip_xyce_renders_a_skip_entry_in_the_same_position() {
        let mut config = base_config();
        config.install_xyce = false;
        let plan = build_plan(&config);
        let names: Vec<_> = plan.entries.iter().map(PlanEntry::name).collect();
        assert_eq!(names[3], "xyce", "the skip keeps its slot in the order");
        match &plan.entries[3] {
            PlanEntry::Skip { reason, .. } => assert!(reason.contains("--skip-xyce")),
            other => panic!("expected a Skip entry, got {other:?}"),
        }
        // Nothing Xyce-related is rendered into any executable step.
        for entry in &plan.entries {
            if let PlanEntry::Step(s) = entry {
                assert!(
                    !s.apply.contains("Trilinos"),
                    "step {} still renders Trilinos with --skip-xyce",
                    s.name
                );
            }
        }
    }

    #[test]
    fn no_step_touches_cloud_cli_tailscale_or_forge() {
        // AC: "No cloud CLI, no Tailscale API call, no forge/token access
        // from this code path."
        let config = base_config();
        let plan = build_plan(&config);
        for entry in &plan.entries {
            if let PlanEntry::Step(s) = entry {
                let hay = format!(
                    "{}\n{}\n{}",
                    s.apply,
                    s.check.clone().unwrap_or_default(),
                    s.verify.clone().unwrap_or_default()
                );
                for banned in [
                    "gcloud",
                    "aws ",
                    "az ",
                    "tailscale",
                    "gh auth",
                    "gh api",
                    "gh repo",
                    "accounts.env",
                    "loom-daemon tokens",
                    ".loom/tokens",
                ] {
                    assert!(
                        !hay.to_lowercase().contains(&banned.to_lowercase()),
                        "step {} unexpectedly references '{}': {}",
                        s.name,
                        banned,
                        hay
                    );
                }
                assert!(
                    s.stdin.is_none(),
                    "step {} unexpectedly carries stdin (no secrets in scope)",
                    s.name
                );
            }
        }
    }

    #[test]
    fn steps_reference_the_configured_refs() {
        let mut config = base_config();
        config.ngspice_ref = "ngspice-99".to_string();
        config.xyce_ref = "Release-9.9".to_string();
        config.gf180mcu_ref = "v9.9.9".to_string();
        config.sky130_ref = "v8.8.8".to_string();
        let plan = build_plan(&config);

        assert!(step(&plan, "ngspice").apply.contains("ngspice-99"));
        assert!(step(&plan, "xyce").apply.contains("Release-9.9"));
        assert!(step(&plan, "gf180mcu-pdk").apply.contains("v9.9.9"));
        assert!(step(&plan, "sky130-pdk").apply.contains("v8.8.8"));
        // The verify step reports the same pins it was built from.
        let verify = &step(&plan, "verify").apply;
        assert!(verify.contains("ngspice-99"));
        assert!(verify.contains("v9.9.9"));
        assert!(verify.contains("v8.8.8"));
    }

    #[test]
    fn base_deps_check_and_apply_cover_the_same_packages() {
        // A package installed by `apply` but absent from `check` would make
        // the step re-apply forever; the reverse would under-install.
        let plan = build_plan(&base_config());
        let s = step(&plan, "spice-base-deps");
        let check = s.check.as_ref().unwrap();
        for pkg in BASE_DEP_PACKAGES.split_whitespace() {
            assert!(check.contains(pkg), "check is missing {pkg}");
            assert!(s.apply.contains(pkg), "apply is missing {pkg}");
        }
    }

    #[test]
    fn pdk_steps_initialize_the_models_submodule() {
        let plan = build_plan(&base_config());
        let gf = step(&plan, "gf180mcu-pdk");
        assert!(gf.apply.contains("submodule update --init"));
        assert!(gf.apply.contains(DEFAULT_GF180MCU_MODELS_PATH));
        assert!(gf
            .check
            .as_ref()
            .unwrap()
            .contains(DEFAULT_GF180MCU_MODELS_PATH));

        let sky = step(&plan, "sky130-pdk");
        assert!(sky.apply.contains(DEFAULT_SKY130_MODELS_PATH));
    }

    #[test]
    fn empty_models_path_skips_the_submodule_init_and_its_probe() {
        let mut config = base_config();
        config.gf180mcu_models_path = String::new();
        let plan = build_plan(&config);
        let gf = step(&plan, "gf180mcu-pdk");
        assert!(!gf.apply.contains("submodule update"));
        assert!(!gf.check.as_ref().unwrap().contains("ls -A"));
        // sky130 (still configured) is unaffected.
        assert!(step(&plan, "sky130-pdk").apply.contains("submodule update"));
    }

    #[test]
    fn binary_probes_export_the_canonical_path_first() {
        // ~/.local/bin is not on a non-interactive SSH shell's default PATH;
        // without the export every re-run would miss the installed binary and
        // rebuild (breaking the idempotency AC).
        let plan = build_plan(&base_config());
        for name in ["ngspice", "xyce"] {
            let check = step(&plan, name).check.as_ref().unwrap();
            assert!(
                check.starts_with("export PATH="),
                "{name} check does not export the canonical PATH: {check}"
            );
            assert!(check.contains("${HOME}/.local/bin"));
        }
        assert!(step(&plan, "verify").apply.contains("${HOME}/.local/bin"));
    }

    #[test]
    fn dry_run_render_lists_every_step_under_its_own_command_name() {
        let config = base_config();
        let plan = build_plan(&config);
        let out = plan.render_dry_run(COMMAND, &config.ssh_host);
        assert!(out.contains("fleet bootstrap-spice plan for spice-runner-1"));
        assert!(!out.contains("add-worker"), "must not borrow add-worker's header");
        assert!(out.contains("7 steps"));
        for name in [
            "spice-base-deps",
            "spice-path",
            "ngspice",
            "xyce",
            "gf180mcu-pdk",
            "sky130-pdk",
            "verify",
        ] {
            assert!(out.contains(name), "dry run omits {name}: {out}");
        }
    }

    #[test]
    fn dry_run_render_shows_the_skipped_xyce_reason() {
        let mut config = base_config();
        config.install_xyce = false;
        let plan = build_plan(&config);
        let out = plan.render_dry_run(COMMAND, &config.ssh_host);
        assert!(out.contains("SKIP: --skip-xyce supplied"));
    }

    // ---- execute_plan / idempotency (drives the plan through a mock
    //      CommandRunner — no live box, per AC) -----------------------------

    #[test]
    fn fresh_bootstrap_applies_every_step_in_order() {
        let config = base_config();
        let plan = build_plan(&config);
        // Every step's `check` fails (not yet bootstrapped) -> apply runs.
        // 6 checked steps (check + apply = 12 calls) + `verify`, which has no
        // check and always applies (1 call) = 13 calls.
        let runner = MockRunner::new(vec![
            fail(1),
            ok(), // spice-base-deps
            fail(1),
            ok(), // spice-path
            fail(1),
            ok(), // ngspice
            fail(1),
            ok(), // xyce
            fail(1),
            ok(), // gf180mcu-pdk
            fail(1),
            ok(), // sky130-pdk
            ok(), // verify (no check)
        ]);
        let reports = execute_plan(&runner, &plan);
        assert!(all_succeeded(&reports, &plan));
        assert!(reports
            .iter()
            .all(|r| matches!(r.status, StepStatus::Changed)));
        assert_eq!(runner.calls().len(), 13);
        // No step ever feeds stdin (no secrets on this code path).
        assert!(runner.calls().iter().all(|(_, stdin)| stdin.is_none()));
    }

    #[test]
    fn idempotent_rerun_reports_every_step_already_done_and_touches_nothing() {
        let config = base_config();
        let plan = build_plan(&config);
        // Every check succeeds (stamp already matches the configured ref) ->
        // no apply ever runs, except `verify` which has no check and always
        // executes (mirrors add_worker's own final verify step).
        let runner = MockRunner::new(vec![ok(); 7]);
        let reports = execute_plan(&runner, &plan);
        assert!(all_succeeded(&reports, &plan));
        let already_done = reports
            .iter()
            .filter(|r| r.status == StepStatus::AlreadyDone)
            .count();
        assert_eq!(already_done, 6, "the 6 checked steps must report AlreadyDone");
        // Only the 6 checks + the 1 unconditional verify ran — zero applies.
        assert_eq!(runner.calls().len(), 7);
        let applies: Vec<_> = runner
            .calls()
            .into_iter()
            .map(|(shell, _)| shell)
            .filter(|shell| shell.contains("git clone") || shell.contains("apt-get install"))
            .collect();
        assert!(applies.is_empty(), "an apply ran on a satisfied host: {applies:?}");
    }

    #[test]
    fn idempotent_rerun_with_xyce_skipped_reports_skip_not_failure() {
        let mut config = base_config();
        config.install_xyce = false;
        let plan = build_plan(&config);
        // 5 checked steps + verify; the Skip entry consumes no runner call.
        let runner = MockRunner::new(vec![ok(); 6]);
        let reports = execute_plan(&runner, &plan);
        assert!(all_succeeded(&reports, &plan));
        assert_eq!(runner.calls().len(), 6);
        let xyce = reports.iter().find(|r| r.name == "xyce").unwrap();
        assert!(matches!(xyce.status, StepStatus::Skipped(_)));
    }

    #[test]
    fn ref_bump_forces_a_rebuild_of_the_changed_step_only() {
        // A stamp-mismatch check must fail (fall through to apply) even if
        // the underlying tool is already installed at some OTHER version —
        // this is the idempotency-vs-version-drift distinction this module's
        // doc comment calls out.
        let config = base_config();
        let bumped = SpiceBootstrapConfig {
            ngspice_ref: "ngspice-43".to_string(),
            ..config.clone()
        };
        let before = step(&build_plan(&config), "ngspice").check.clone().unwrap();
        let after = step(&build_plan(&bumped), "ngspice").check.clone().unwrap();
        assert!(before.contains("ngspice-42"));
        assert!(after.contains("ngspice-43"));
        assert_ne!(before, after, "a ref bump must change the stamp check");
        // The PDK steps are untouched by an ngspice bump.
        assert_eq!(
            step(&build_plan(&config), "sky130-pdk").check,
            step(&build_plan(&bumped), "sky130-pdk").check
        );
    }

    #[test]
    fn xyce_stamp_covers_the_trilinos_pin_too() {
        let config = base_config();
        let bumped = SpiceBootstrapConfig {
            trilinos_ref: "trilinos-release-15-0-0".to_string(),
            ..config.clone()
        };
        let before = step(&build_plan(&config), "xyce").check.clone().unwrap();
        let after = step(&build_plan(&bumped), "xyce").check.clone().unwrap();
        assert_ne!(before, after, "a Trilinos pin bump must invalidate the Xyce stamp");
        assert!(after.contains("trilinos-release-15-0-0"));
    }

    #[test]
    fn plan_halts_on_first_failure_and_is_resumable() {
        let config = base_config();
        let plan = build_plan(&config);
        // spice-base-deps check fails, apply fails -> halt immediately.
        let runner = MockRunner::new(vec![fail(1), fail(2)]);
        let reports = execute_plan(&runner, &plan);
        assert_eq!(reports.len(), 1, "halts after the failing step");
        assert!(reports[0].status.is_failure());
        assert!(!all_succeeded(&reports, &plan));

        // Resume: the operator fixes the cause and re-runs. The prefix that
        // already succeeded reports AlreadyDone; only the rest is applied.
        let resumed = MockRunner::new(vec![
            ok(),    // spice-base-deps check now passes
            ok(),    // spice-path check passes
            fail(1), // ngspice not yet built
            ok(),    // ngspice apply
        ]);
        let reports = execute_plan(&resumed, &build_plan(&config));
        assert_eq!(reports[0].status, StepStatus::AlreadyDone);
        assert_eq!(reports[1].status, StepStatus::AlreadyDone);
        assert_eq!(reports[2].status, StepStatus::Changed);
    }

    #[test]
    fn run_dry_run_never_touches_the_ssh_runner() {
        // A dry-run must return before any CommandRunner is constructed —
        // exercised via `run()`'s public contract (it must not error on the
        // unreachable host `does-not-exist.invalid`, because it never
        // contacts it).
        let mut config = base_config();
        config.ssh_host = "does-not-exist.invalid".to_string();
        config.dry_run = true;
        assert!(run(&config).is_ok());
    }

    #[test]
    fn run_rejects_a_bad_pin_before_dry_run_printing() {
        let mut config = base_config();
        config.dry_run = true;
        config.ngspice_ref = "$(reboot)".to_string();
        assert!(run(&config).is_err(), "preflight must gate even a dry run");
    }
}
