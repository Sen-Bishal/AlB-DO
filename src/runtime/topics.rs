//! PRISM § 4 · the resolver — one function, three callers.
//!
//! A [`PartitionTopicSpec`] is what the compiler knows: a collection, the column
//! it is partitioned by, and where the key comes from. A [`ResolvedPartition`]
//! is what a *request* knows: that spec plus the key the router matched, and the
//! topic identity minted from the two.
//!
//! Invariant 5 is the reason this is a module and not three call sites. Render,
//! subscribe and write must derive the same identity or they drift into three
//! different channels — the page rendering from one, the subscriber listening on
//! another, the write fanning out on a third. Each is individually plausible and
//! the composite failure is "the row appeared for nobody". So the minting rule
//! lives in [`partition_topic_name`] and the *resolution* rule lives here, and
//! neither is reimplemented anywhere.
//!
//! ## Why an unresolvable spec is not an error
//!
//! A key that fails validation, or a param the route never matched, yields **no
//! topic** — not a failed render. PRISM § 4: a weird id in a URL must not take
//! the page down. The route degrades to a static page with an empty slot, which
//! is [`crate::runtime::TopicIdentity`]-consistent (nothing was minted, so
//! nothing can be subscribed to) and recoverable by fixing the URL.

use crate::aperture::SourceRegistry;
use crate::manifest::schema::{PartitionTopicSpec, SourceArgSpec, SourceTopicSpec};
use crate::runtime::broadcast::partition_topic_name;

/// One partition spec resolved against a request's route params.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPartition {
    /// The component-local binding name — the key `__albedo_topic("…")` reads
    /// out of `host.topics` during a Tier-B render.
    pub binding: String,
    /// The minted topic identity, `"{collection}:{key}"`. The wire slot id is
    /// [`crate::runtime::broadcast_slot_id`] of this, computed independently by
    /// the client from the same string the server stamps into the DOM.
    pub topic: String,
    /// The declared collection this partitions.
    pub collection: String,
    /// The validated partition key.
    pub key: String,
}

/// Resolve every spec that this request supplies a usable key for.
///
/// `param` is the caller's own params lookup — the router's matched map on the
/// subscribe path, the `params` prop on the render path. Both descend from the
/// same `RouteMatch`, so passing the lookup rather than a concrete map keeps one
/// resolver without forcing the two callers to agree on a container type.
///
/// Specs that resolve to nothing are **skipped silently here** and reported by
/// the caller that has somewhere to report to (the render path logs a dev-mode
/// warning; the subscribe path simply grants fewer topics). Returning them as
/// errors would push a URL-shaped mistake into a page-shaped failure.
///
/// Output order follows `specs`, which the manifest builder sorts — so a route's
/// topic list is stable across builds and a diff of two manifests is readable.
pub fn resolve_partition_topics<'a, F>(
    specs: &[PartitionTopicSpec],
    param: F,
) -> Vec<ResolvedPartition>
where
    F: Fn(&str) -> Option<&'a str>,
{
    specs
        .iter()
        .filter_map(|spec| {
            let key = param(spec.param.as_str())?;
            let topic = partition_topic_name(spec.collection.as_str(), key)?;
            Some(ResolvedPartition {
                binding: spec.binding.clone(),
                topic,
                collection: spec.collection.clone(),
                key: key.to_string(),
            })
        })
        .collect()
}

/// APERTURE · one source binding resolved against a request's route params.
///
/// The analogue of [`ResolvedPartition`], carrying the extra thing a remote
/// derivation needs and a local one does not: the URL to actually fetch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSourceTopic {
    /// The component-local binding name — the key `__albedo_topic("…")` reads
    /// out of `host.topics` during a Tier-B render.
    pub binding: String,
    /// The minted topic identity, `aperture:{source}.{route}[:args]`.
    pub topic: String,
    /// The declared source.
    pub source: String,
    /// The declared route.
    pub route: String,
    /// The absolute URL this topic derives from.
    pub url: String,
}

/// Resolve every source spec this request supplies usable arguments for.
///
/// The APERTURE half of PRISM invariant 5 — *one resolver*. Render and subscribe
/// both call this, so a page cannot render from one identity while its lane
/// listens on another.
///
/// Resolution is delegated to
/// [`SourceRoute::resolve`](crate::aperture::SourceRoute::resolve) rather than
/// reimplemented, so the URL and the identity are always built from the same
/// template walk, in template order. A spec naming a route the registry does not
/// have — or one whose arguments do not satisfy the key alphabet — yields **no
/// topic**, not an error, exactly as a partition does: PRISM § 4's rule that a
/// weird value in a URL must not take the page down.
pub fn resolve_source_topics<'a, F>(
    specs: &[SourceTopicSpec],
    registry: &SourceRegistry,
    param: F,
) -> Vec<ResolvedSourceTopic>
where
    F: Fn(&str) -> Option<&'a str>,
{
    specs
        .iter()
        .filter_map(|spec| {
            let route = registry.get(&spec.source, &spec.route)?;
            // The spec's own arguments are the lookup the route resolves
            // against: a literal answers itself, a param defers to the request.
            let resolved = route.resolve(|name| {
                spec.args.iter().find(|arg| arg.name() == name).and_then(
                    |arg| match arg {
                        SourceArgSpec::Literal { value, .. } => Some(value.as_str()),
                        SourceArgSpec::Param { param: from, .. } => param(from.as_str()),
                    },
                )
            })?;
            Some(ResolvedSourceTopic {
                binding: spec.binding.clone(),
                topic: resolved.topic,
                source: resolved.source,
                route: resolved.route,
                url: resolved.url,
            })
        })
        .collect()
}

/// The inverse of [`partition_topic_name`]: split a minted topic back into its
/// `(collection, key)`.
///
/// It lives here, beside the composition it undoes, for the reason invariant 5
/// exists — two functions that must agree about a format belong in one file
/// where changing one without the other is obviously wrong.
///
/// The split is **exact**, not a heuristic, and only because the key alphabet
/// forbids `:`. Splitting at the last colon therefore always recovers the pair
/// that produced the name — the same property § 3.2 relies on to make two
/// partitions aliasing onto one channel unexpressible.
///
/// It cannot, however, tell a partition from a *static* topic that merely
/// contains a colon (`broadcast("chat:lobby")` is legal and predates all of
/// this). So callers must try the whole name as itself first and reach for this
/// only when that fails — the ordering is the disambiguation, and this function
/// does not attempt it alone.
#[must_use]
pub fn split_partition_topic(topic: &str) -> Option<(&str, &str)> {
    let (collection, key) = topic.rsplit_once(':')?;
    (!collection.is_empty() && crate::runtime::broadcast::is_valid_partition_key(key))
        .then_some((collection, key))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn spec(binding: &str, collection: &str, column: &str, param: &str) -> PartitionTopicSpec {
        PartitionTopicSpec {
            binding: binding.to_string(),
            collection: collection.to_string(),
            column: column.to_string(),
            param: param.to_string(),
        }
    }

    fn params(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn a_matched_param_mints_the_canonical_identity() {
        let specs = vec![spec("rows", "messages", "room", "id")];
        let bound = params(&[("id", "42")]);
        let resolved = resolve_partition_topics(&specs, |name| {
            bound.get(name).map(String::as_str)
        });

        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].topic, "messages:42");
        assert_eq!(resolved[0].binding, "rows");
        assert_eq!(resolved[0].collection, "messages");
        assert_eq!(resolved[0].key, "42");
    }

    /// The route matched, but not with the param this binding names — so the
    /// binding has no key and the page renders without live data rather than
    /// failing. PRISM § 4.
    #[test]
    fn an_unmatched_param_resolves_to_nothing() {
        let specs = vec![spec("rows", "messages", "room", "id")];
        let bound = params(&[("slug", "42")]);
        assert!(resolve_partition_topics(&specs, |name| bound
            .get(name)
            .map(String::as_str))
        .is_empty());
    }

    /// A URL segment is attacker-controlled. It reaches a topic namespace and a
    /// SQL parameter, so a key outside the alphabet must produce no topic at
    /// all — not a sanitized one, which would silently answer a different
    /// question than the one asked.
    #[test]
    fn a_key_outside_the_alphabet_resolves_to_nothing() {
        let specs = vec![spec("rows", "messages", "room", "id")];
        for hostile in ["a:b", "", "../etc", "a b", &"x".repeat(65)] {
            let bound = params(&[("id", hostile)]);
            assert!(
                resolve_partition_topics(&specs, |name| bound.get(name).map(String::as_str))
                    .is_empty(),
                "key {hostile:?} must not mint a topic"
            );
        }
    }

    /// The render and subscribe paths call this with different containers but
    /// the same underlying matched params. If they could disagree, a page would
    /// render from one channel while its lane listened on another — the exact
    /// drift invariant 5 exists to prevent.
    #[test]
    fn the_same_params_resolve_identically_whatever_the_container() {
        let specs = vec![
            spec("rows", "messages", "room", "id"),
            spec("notes", "notes", "room", "id"),
        ];
        let hash: HashMap<String, String> = params(&[("id", "room_7")]);
        let tree: std::collections::BTreeMap<String, String> = hash.clone().into_iter().collect();

        let from_hash =
            resolve_partition_topics(&specs, |name| hash.get(name).map(String::as_str));
        let from_tree =
            resolve_partition_topics(&specs, |name| tree.get(name).map(String::as_str));

        assert_eq!(from_hash, from_tree);
        assert_eq!(
            from_hash.iter().map(|r| r.topic.as_str()).collect::<Vec<_>>(),
            ["messages:room_7", "notes:room_7"]
        );
    }

    /// Round-trip: whatever `partition_topic_name` mints, `split_partition_topic`
    /// must take apart into the pair that produced it. These two are the only
    /// code that knows the format, and this is the property that lets them stay
    /// that way.
    #[test]
    fn minting_and_splitting_are_inverses() {
        for (collection, key) in [
            ("messages", "42"),
            ("messages", "room_a-1"),
            // A collection name carrying the separator is still recovered
            // exactly, because the key never can.
            ("x:1", "2"),
        ] {
            let minted = partition_topic_name(collection, key).expect("mints");
            assert_eq!(split_partition_topic(&minted), Some((collection, key)));
        }
    }

    // ── APERTURE · the source resolver ───────────────────────────────────

    fn source_registry() -> crate::aperture::SourceRegistry {
        use crate::aperture::{RouteDecl, SourceDecl};
        let mut routes = std::collections::BTreeMap::new();
        routes.insert(
            "repo".to_string(),
            RouteDecl {
                path: "/repos/{owner}/{name}".to_string(),
                refresh: None,
                method: None,
            },
        );
        let decls: std::collections::BTreeMap<String, SourceDecl> = [(
            "github".to_string(),
            SourceDecl {
                base: "https://api.github.com".to_string(),
                auth: None,
                headers: std::collections::BTreeMap::new(),
                routes,
            },
        )]
        .into_iter()
        .collect();
        crate::aperture::SourceRegistry::from_declarations(&decls, |_| None).expect("lowers")
    }

    fn source_spec(args: Vec<SourceArgSpec>) -> SourceTopicSpec {
        SourceTopicSpec {
            binding: "repo".to_string(),
            source: "github".to_string(),
            route: "repo".to_string(),
            args,
        }
    }

    #[test]
    fn a_source_spec_resolves_to_a_topic_and_a_url() {
        let specs = vec![source_spec(vec![
            SourceArgSpec::Literal {
                name: "name".to_string(),
                value: "claude-code".to_string(),
            },
            SourceArgSpec::Param {
                name: "owner".to_string(),
                param: "org".to_string(),
            },
        ])];
        let bound = params(&[("org", "anthropics")]);
        let resolved = resolve_source_topics(&specs, &source_registry(), |name| {
            bound.get(name).map(String::as_str)
        });

        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].binding, "repo");
        assert_eq!(
            resolved[0].topic,
            "aperture:github.repo:owner=anthropics,name=claude-code"
        );
        assert_eq!(
            resolved[0].url,
            "https://api.github.com/repos/anthropics/claude-code"
        );
    }

    /// The identity must follow the **route template's** order, not the spec's.
    /// Otherwise the same resource reached from two components with differently
    /// ordered object literals would mint two topics and fetch twice.
    #[test]
    fn identity_order_follows_the_template_not_the_spec() {
        let forward = vec![source_spec(vec![
            SourceArgSpec::Literal {
                name: "name".to_string(),
                value: "b".to_string(),
            },
            SourceArgSpec::Literal {
                name: "owner".to_string(),
                value: "a".to_string(),
            },
        ])];
        let reverse = vec![source_spec(vec![
            SourceArgSpec::Literal {
                name: "owner".to_string(),
                value: "a".to_string(),
            },
            SourceArgSpec::Literal {
                name: "name".to_string(),
                value: "b".to_string(),
            },
        ])];
        let registry = source_registry();
        let one = resolve_source_topics(&forward, &registry, |_| None);
        let two = resolve_source_topics(&reverse, &registry, |_| None);
        assert_eq!(one, two);
        assert_eq!(one[0].topic, "aperture:github.repo:owner=a,name=b");
    }

    /// PRISM § 4's rule, inherited: an unmatched param yields no topic, so the
    /// page renders static rather than failing.
    #[test]
    fn an_unmatched_param_resolves_to_no_source_topic() {
        let specs = vec![source_spec(vec![
            SourceArgSpec::Param {
                name: "owner".to_string(),
                param: "org".to_string(),
            },
            SourceArgSpec::Literal {
                name: "name".to_string(),
                value: "b".to_string(),
            },
        ])];
        let bound = params(&[("other", "x")]);
        assert!(
            resolve_source_topics(&specs, &source_registry(), |name| bound
                .get(name)
                .map(String::as_str))
            .is_empty()
        );
    }

    #[test]
    fn a_hostile_param_resolves_to_no_source_topic() {
        let registry = source_registry();
        for hostile in ["../../etc", "a/b", "a?x=1", "a#f", "", &"x".repeat(65)] {
            let specs = vec![source_spec(vec![
                SourceArgSpec::Param {
                    name: "owner".to_string(),
                    param: "org".to_string(),
                },
                SourceArgSpec::Literal {
                    name: "name".to_string(),
                    value: "b".to_string(),
                },
            ])];
            let bound = params(&[("org", hostile)]);
            assert!(
                resolve_source_topics(&specs, &registry, |name| bound
                    .get(name)
                    .map(String::as_str))
                .is_empty(),
                "param {hostile:?} must not resolve"
            );
        }
    }

    /// A spec naming a route the registry does not have contributes nothing
    /// rather than panicking — the boot check is what makes this loud, and by
    /// the time a request runs the mismatch is already a build failure.
    #[test]
    fn a_spec_for_an_undeclared_route_resolves_to_nothing() {
        let mut spec = source_spec(vec![]);
        spec.route = "issues".to_string();
        assert!(resolve_source_topics(&[spec], &source_registry(), |_| None).is_empty());
    }

    /// Render and subscribe call this with different containers over the same
    /// matched params. Disagreement here is the drift PRISM invariant 5 forbids.
    #[test]
    fn the_same_params_resolve_identically_whatever_the_container_for_sources() {
        let specs = vec![source_spec(vec![
            SourceArgSpec::Param {
                name: "owner".to_string(),
                param: "org".to_string(),
            },
            SourceArgSpec::Literal {
                name: "name".to_string(),
                value: "b".to_string(),
            },
        ])];
        let registry = source_registry();
        let hash: HashMap<String, String> = params(&[("org", "anthropics")]);
        let tree: std::collections::BTreeMap<String, String> = hash.clone().into_iter().collect();

        assert_eq!(
            resolve_source_topics(&specs, &registry, |n| hash.get(n).map(String::as_str)),
            resolve_source_topics(&specs, &registry, |n| tree.get(n).map(String::as_str))
        );
    }

    #[test]
    fn a_name_that_could_not_have_been_minted_does_not_split() {
        assert_eq!(split_partition_topic("guestbook"), None);
        assert_eq!(split_partition_topic("messages:"), None);
        assert_eq!(split_partition_topic(":42"), None);
        assert_eq!(split_partition_topic("messages:a/b"), None);
    }

    /// One bad spec must not suppress the good ones beside it: a route reading
    /// two partitions where only one key is present still gets the one.
    #[test]
    fn resolution_is_per_spec_not_all_or_nothing() {
        let specs = vec![
            spec("rows", "messages", "room", "id"),
            spec("docs", "documents", "project", "projectId"),
        ];
        let bound = params(&[("id", "9")]);
        let resolved =
            resolve_partition_topics(&specs, |name| bound.get(name).map(String::as_str));

        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].topic, "messages:9");
    }
}
