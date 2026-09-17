//! `export const config = { matcher }` — which requests enter userland.
//!
//! ## Why this is compiled, not evaluated
//!
//! The matcher is the one part of a middleware that runs on **every** request,
//! including every image on every page. Evaluating it in QuickJS would put an
//! engine checkout in front of each of those; reading it out of the AST at build
//! time puts a few string comparisons there instead, and a path the matcher
//! rejects never reaches an engine at all.
//!
//! ## The accepted syntax, and why it is a subset
//!
//! The spelling is Next.js's, so a pasted `middleware.ts` reads the way its
//! author expects. The *semantics* are a strict subset of `path-to-regexp`:
//!
//! | pattern | matches |
//! |---|---|
//! | `/about` | exactly `/about` (a trailing slash is ignored) |
//! | `/blog/:slug` | one non-empty segment |
//! | `/docs/:rest?` | zero or one segment, last position only |
//! | `/admin/:path*` | zero or more segments, last position only |
//! | `/api/:path+` | one or more segments, last position only |
//!
//! Everything else — regex groups, `(.*)`, bare `*`, `{}` optional groups, the
//! object form with `has`/`missing` — is **refused at build**, naming the
//! construct. The alternative is a pattern that parses as something narrower
//! than its author meant, and a middleware that silently does not run is
//! exactly the failure a guard written in it cannot survive.
//!
//! ## Decoded, on purpose
//!
//! A pattern is compared against the **percent-decoded** path. The router
//! matches the raw one, so `/%61dmin` never reaches the `/admin` route — but a
//! matcher that compared raw bytes would decide "this is not `/admin`" and skip
//! the middleware for a request that some other lane (a `public/` file lookup, a
//! future decoding router) could still serve as `/admin`. Decoding here can only
//! make middleware run on *more* requests, never fewer, which is the safe
//! direction for a thing people will write guards in.

use serde::{Deserialize, Serialize};

/// One path segment of a compiled pattern.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
enum Segment {
    /// Compared byte-for-byte against the decoded segment.
    Literal(String),
    /// `:name` — exactly one non-empty segment.
    Param,
    /// `:name?` — zero or one segment. Last position only.
    Optional,
    /// `:name*` (`at_least_one = false`) or `:name+` (`true`). Last position only.
    Rest { at_least_one: bool },
}

/// One compiled pattern, with its source kept for error messages and doctor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pattern {
    source: String,
    segments: Vec<Segment>,
}

/// A middleware's matcher. **`None` patterns means every request**, which is
/// what a middleware with no `config` does in Next.js too.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Matcher {
    patterns: Option<Vec<Pattern>>,
}

impl Matcher {
    /// The matcher a middleware with no `config.matcher` gets: everything.
    #[must_use]
    pub fn all() -> Self {
        Self { patterns: None }
    }

    /// Compile every pattern, collecting every failure rather than the first.
    ///
    /// # Errors
    /// One message per refused pattern, each naming it.
    pub fn compile<S: AsRef<str>>(sources: &[S]) -> Result<Self, Vec<String>> {
        let mut patterns = Vec::with_capacity(sources.len());
        let mut problems = Vec::new();
        for source in sources {
            match Pattern::parse(source.as_ref()) {
                Ok(pattern) => patterns.push(pattern),
                Err(message) => problems.push(message),
            }
        }
        if !problems.is_empty() {
            return Err(problems);
        }
        if patterns.is_empty() {
            // `matcher: []` is not "everything" — it is a middleware that can
            // never run, which is a mistake the author cannot see from the
            // outside. Refused rather than guessed at either way.
            return Err(vec![
                "`config.matcher` is an empty list, so this middleware could never run. Remove \
                 `matcher` to run on every request, or list the paths it should run on"
                    .to_string(),
            ]);
        }
        Ok(Self {
            patterns: Some(patterns),
        })
    }

    /// Whether a request for `raw_path` (as it arrived, still percent-encoded)
    /// enters the middleware.
    #[must_use]
    pub fn matches(&self, raw_path: &str) -> bool {
        let Some(patterns) = &self.patterns else {
            return true;
        };
        let decoded = percent_decode_path(raw_path);
        let segments: Vec<&str> = decoded.split('/').filter(|s| !s.is_empty()).collect();
        patterns.iter().any(|pattern| pattern.matches_segments(&segments))
    }

    /// The pattern sources, for diagnostics. `None` when every request matches.
    #[must_use]
    pub fn sources(&self) -> Option<Vec<&str>> {
        self.patterns
            .as_ref()
            .map(|patterns| patterns.iter().map(|p| p.source.as_str()).collect())
    }
}

impl Pattern {
    /// Parse one Next-style pattern.
    ///
    /// # Errors
    /// A message naming the pattern and the construct that is not supported.
    pub fn parse(source: &str) -> Result<Self, String> {
        let refuse = |why: &str| Err(format!("matcher \"{source}\": {why}"));

        if !source.starts_with('/') {
            return refuse("a pattern must start with `/`");
        }
        if let Some(bad) = source
            .chars()
            .find(|c| matches!(c, '(' | ')' | '[' | ']' | '{' | '}' | '\\' | '|' | '^' | '$'))
        {
            return refuse(&format!(
                "`{bad}` is regex syntax, which is not supported. Use `:name` for one segment \
                 and `:name*` / `:name+` for the rest of the path"
            ));
        }

        let parts: Vec<&str> = source.split('/').filter(|s| !s.is_empty()).collect();
        let mut segments = Vec::with_capacity(parts.len());
        for (index, part) in parts.iter().enumerate() {
            let last = index + 1 == parts.len();
            let segment = if let Some(param) = part.strip_prefix(':') {
                let (name, kind) = match param.chars().last() {
                    Some('*') => (&param[..param.len() - 1], Segment::Rest { at_least_one: false }),
                    Some('+') => (&param[..param.len() - 1], Segment::Rest { at_least_one: true }),
                    Some('?') => (&param[..param.len() - 1], Segment::Optional),
                    _ => (param, Segment::Param),
                };
                if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                    return refuse(&format!(
                        "`:{param}` is not a parameter — a name is letters, digits and `_`, \
                         optionally followed by one of `?`, `*` or `+`"
                    ));
                }
                if !last && kind != Segment::Param {
                    return refuse(&format!(
                        "`:{param}` can only be the last segment — nothing after a variable-length \
                         segment could be matched unambiguously"
                    ));
                }
                kind
            } else {
                if let Some(bad) = part.chars().find(|c| matches!(c, '*' | '+' | '?' | ':')) {
                    return refuse(&format!(
                        "`{bad}` inside the segment `{part}` is not supported. A wildcard is its \
                         own segment: write `/:path*` rather than `/*` or `/foo*`"
                    ));
                }
                if *part == "." || *part == ".." {
                    return refuse("`.` and `..` segments can never match a request path");
                }
                Segment::Literal((*part).to_string())
            };
            segments.push(segment);
        }

        Ok(Self {
            source: source.to_string(),
            segments,
        })
    }

    fn matches_segments(&self, path: &[&str]) -> bool {
        let mut index = 0;
        for segment in &self.segments {
            match segment {
                Segment::Literal(literal) => {
                    if path.get(index) != Some(&literal.as_str()) {
                        return false;
                    }
                    index += 1;
                }
                Segment::Param => {
                    if path.get(index).is_none() {
                        return false;
                    }
                    index += 1;
                }
                // Last position is enforced at parse, so these consume the rest.
                Segment::Optional => return path.len() - index <= 1,
                Segment::Rest { at_least_one } => {
                    return !*at_least_one || path.len() > index;
                }
            }
        }
        index == path.len()
    }
}

/// Percent-decode a URL path. Invalid escapes are kept verbatim and invalid
/// UTF-8 is replaced — the result is only ever compared, never served.
///
/// Not `form_urlencoded`: that decodes `+` to a space, which is right for a
/// query string and wrong for a path.
#[must_use]
pub fn percent_decode_path(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = |b: u8| (b as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(patterns: &[&str]) -> Matcher {
        Matcher::compile(patterns).expect("compiles")
    }

    #[test]
    fn no_matcher_means_every_request() {
        let all = Matcher::all();
        for path in ["/", "/logo.png", "/a/b/c", ""] {
            assert!(all.matches(path), "{path}");
        }
    }

    #[test]
    fn a_literal_matches_itself_and_ignores_a_trailing_slash() {
        let matcher = m(&["/about"]);
        assert!(matcher.matches("/about"));
        assert!(matcher.matches("/about/"));
        assert!(!matcher.matches("/about/team"));
        assert!(!matcher.matches("/aboutx"));
        assert!(!matcher.matches("/"));
    }

    #[test]
    fn the_root_pattern_matches_only_the_root() {
        let matcher = m(&["/"]);
        assert!(matcher.matches("/"));
        assert!(!matcher.matches("/a"));
    }

    #[test]
    fn a_param_is_exactly_one_segment() {
        let matcher = m(&["/blog/:slug"]);
        assert!(matcher.matches("/blog/hello"));
        assert!(!matcher.matches("/blog"));
        assert!(!matcher.matches("/blog/a/b"));
    }

    #[test]
    fn star_is_zero_or_more_and_plus_is_one_or_more() {
        let star = m(&["/admin/:path*"]);
        assert!(star.matches("/admin"));
        assert!(star.matches("/admin/users"));
        assert!(star.matches("/admin/users/7/edit"));
        assert!(!star.matches("/administrator"));

        let plus = m(&["/api/:path+"]);
        assert!(!plus.matches("/api"));
        assert!(plus.matches("/api/x"));
        assert!(plus.matches("/api/x/y"));
    }

    #[test]
    fn optional_is_zero_or_one() {
        let matcher = m(&["/docs/:page?"]);
        assert!(matcher.matches("/docs"));
        assert!(matcher.matches("/docs/intro"));
        assert!(!matcher.matches("/docs/intro/more"));
    }

    #[test]
    fn any_pattern_in_the_list_admits_the_request() {
        let matcher = m(&["/admin/:path*", "/account"]);
        assert!(matcher.matches("/account"));
        assert!(matcher.matches("/admin/x"));
        assert!(!matcher.matches("/public"));
    }

    /// 🔑 The property a guard written in middleware depends on: an encoded
    /// spelling of a matched path still enters the middleware.
    #[test]
    fn an_encoded_path_cannot_step_around_the_matcher() {
        let matcher = m(&["/admin/:path*"]);
        assert!(matcher.matches("/%61dmin"));
        assert!(matcher.matches("/%61%64%6D%69%6E/users"));
    }

    #[test]
    fn plus_is_not_decoded_to_space_in_a_path() {
        assert_eq!(percent_decode_path("/a+b%20c"), "/a+b c");
        assert_eq!(percent_decode_path("/100%"), "/100%");
        assert_eq!(percent_decode_path("/%zz"), "/%zz");
        assert_eq!(percent_decode_path("/%4"), "/%4");
    }

    #[test]
    fn regex_and_wildcard_spellings_are_refused_by_name() {
        for (pattern, needle) in [
            ("/((?!api).*)", "regex"),
            ("/admin/(.*)", "regex"),
            ("/admin/*", "own segment"),
            ("/foo*", "own segment"),
            ("admin", "start with `/`"),
            ("/:path*/edit", "last segment"),
            ("/:", "not a parameter"),
            ("/:a-b", "not a parameter"),
            ("/a/../b", "can never match"),
        ] {
            let err = Pattern::parse(pattern).expect_err(pattern);
            assert!(err.contains(needle), "{pattern}: {err}");
            assert!(err.contains(pattern), "the refusal names the pattern: {err}");
        }
    }

    #[test]
    fn every_bad_pattern_is_reported_not_just_the_first() {
        let problems = Matcher::compile(&["/ok", "/(a)", "/b/*"]).expect_err("two are bad");
        assert_eq!(problems.len(), 2, "{problems:?}");
    }

    #[test]
    fn an_empty_list_is_refused_rather_than_read_as_everything_or_nothing() {
        let problems = Matcher::compile::<&str>(&[]).expect_err("empty");
        assert!(problems[0].contains("could never run"));
    }
}
