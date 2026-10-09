//! The conditional `inbox_mail` health section (#10137): a host that looks
//! meant to send operator mail (observability endpoint or `LOOM_UI_INBOX_URL`
//! configured) but cannot resolve the inbox URL or ingest key is reported on
//! every `loom-daemon health` run, not only when someone tries to send.
//! Split out because `health.rs` sits at its file-size ratchet.

use super::{HealthInputs, HealthSection, Verdict};

/// No section unless collected (the collector already filters to mail-meant,
/// unresolved hosts). The detail carries paths and names only, never a key.
#[must_use]
pub fn assess_inbox_mail(inputs: &HealthInputs) -> Option<HealthSection> {
    let r = inputs.inbox_mail.as_ref()?;
    if !r.mail_meant || r.missing.is_empty() {
        return None;
    }
    Some(HealthSection::new(
        "inbox_mail",
        Verdict::Degraded,
        format!(
            "/loom:mail-send cannot send from this host -- unresolved: {}",
            r.missing.join("; ")
        ),
        serde_json::to_value(r).unwrap_or_default(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inbox_config::InboxResolution;

    fn base(inbox_mail: Option<InboxResolution>) -> HealthInputs {
        HealthInputs {
            inbox_mail,
            ..Default::default()
        }
    }

    #[test]
    fn degraded_and_named_when_mail_meant_and_unresolved() {
        let s = assess_inbox_mail(&base(Some(InboxResolution {
            mail_meant: true,
            missing: vec!["ingest key: /k is missing".into()],
            ..Default::default()
        })))
        .expect("section");
        assert_eq!(s.verdict, Verdict::Degraded);
        assert!(s.summary.contains("/k is missing"));
    }

    #[test]
    fn silent_when_not_collected_or_not_mail_meant() {
        assert!(assess_inbox_mail(&base(None)).is_none());
        assert!(assess_inbox_mail(&base(Some(InboxResolution::default()))).is_none());
    }
}
