//! Where an action may send the browser — the `redirects` block, enforced.
//!
//! `return { redirect: url }` from an action becomes a `Navigate`. Before any
//! transport sends one, the action adapter asks [`RedirectPolicy::check`]:
//!
//! - **A same-origin path** (`/account/welcome`) always passes, under the same
//!   rules as a form's return path ([`crate::forms::ReturnPath`]) — rooted, not
//!   protocol-relative, no backslashes or control characters.
//! - **Anything else** passes only when its origin is listed in the config's
//!   `redirects`. A checkout page, an identity provider: declared, by origin.
//!
//! The refusal is an error, not a quiet fallback to `/`. An action that computes
//! a redirect from input — a `next` field, an upstream response — and meets this
//! check has a bug worth hearing about, and silently landing somewhere else
//! would hide it.

use std::collections::BTreeSet;

/// The origins actions may redirect to, beyond this one.
#[derive(Debug, Clone, Default)]
pub struct RedirectPolicy {
    origins: BTreeSet<String>,
}

impl RedirectPolicy {
    /// Build from the contract's `redirects`, canonicalizing each entry.
    ///
    /// # Errors
    /// An entry that is not an `https` origin — already refused by the build,
    /// and refused again here because a server can be handed options directly.
    pub fn new(origins: &[String]) -> Result<Self, String> {
        let origins = origins
            .iter()
            .map(|raw| dom_render_compiler::dev_contract::parse_redirect_origin(raw))
            .collect::<Result<BTreeSet<_>, _>>()?;
        Ok(Self { origins })
    }

    /// Whether an action may send the browser to `url`.
    ///
    /// # Errors
    /// Why not — naming the origin to list, when that is the fix.
    pub fn check(&self, url: &str) -> Result<(), String> {
        if url.starts_with('/') {
            return crate::forms::ReturnPath::parse(url).map(|_| ()).ok_or_else(|| {
                "a same-origin redirect must be a plain path like \"/account\"".to_string()
            });
        }
        let parsed = url::Url::parse(url)
            .map_err(|_| "a redirect is a path (\"/account\") or an https URL".to_string())?;
        let origin = parsed.origin().ascii_serialization();
        if parsed.scheme() == "https" && self.origins.contains(&origin) {
            return Ok(());
        }
        Err(format!(
            "`{origin}` is not in `redirects` — add it to albedo.config.ts: \
             `redirects: [\"{origin}\"]`"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> RedirectPolicy {
        RedirectPolicy::new(&["https://checkout.stripe.com".to_string()]).unwrap()
    }

    #[test]
    fn a_same_origin_path_always_passes_and_a_protocol_relative_one_never_does() {
        assert!(policy().check("/account/welcome?x=1").is_ok());
        assert!(RedirectPolicy::default().check("/account").is_ok());
        assert!(policy().check("//evil.example/x").is_err());
        assert!(policy().check("/\\evil.example").is_err());
    }

    #[test]
    fn another_origin_passes_only_when_listed_and_only_over_https() {
        assert!(policy().check("https://checkout.stripe.com/c/pay/cs_1#x").is_ok());
        assert!(policy().check("https://evil.example/pay").is_err());
        assert!(policy().check("http://checkout.stripe.com/pay").is_err());
        assert!(policy().check("javascript:alert(1)").is_err());
        let refusal = policy().check("https://evil.example/").unwrap_err();
        assert!(refusal.contains("redirects: [\"https://evil.example\"]"), "{refusal}");
    }

    #[test]
    fn a_listed_entry_must_be_an_https_origin() {
        assert!(RedirectPolicy::new(&["http://x.example".to_string()]).is_err());
        assert!(RedirectPolicy::new(&["https://x.example/path".to_string()]).is_err());
        assert!(RedirectPolicy::new(&["https://X.Example:443".to_string()])
            .unwrap()
            .check("https://x.example/a")
            .is_ok());
    }
}
