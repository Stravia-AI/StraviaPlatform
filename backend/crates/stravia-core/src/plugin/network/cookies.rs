//! Process-memory cookie state for an installation/vendor/provider epoch.
//!
//! The lifecycle registry owns the jar. Callers must hold its read guard while
//! using it, and obtain a fresh jar after checking operation freshness per hop.

use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Mutex;

use cookie_store::{Cookie, CookieError, CookieStore};
use stravia_vendor_sdk::net::{CookiePolicy, Field};
use url::Url;

const MAX_COOKIES: usize = 256;
const MAX_BYTES: usize = 256 * 1024;

#[derive(Default)]
pub(crate) struct CookieJar {
    store: Mutex<CookieStore>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CookieLimit;

fn matching_url<'a>(policy: &CookiePolicy, url: &'a Url) -> Option<Cow<'a, Url>> {
    if !matches!(url.scheme(), "https" | "wss")
        || !url
            .host_str()
            .is_some_and(|host| policy.domains.iter().any(|domain| domain == host))
    {
        return None;
    }
    if url.scheme() == "wss" {
        let mut normalized = url.clone();
        normalized.set_scheme("https").ok()?;
        Some(Cow::Owned(normalized))
    } else {
        Some(Cow::Borrowed(url))
    }
}

fn allowed_name(policy: &CookiePolicy, name: &str) -> bool {
    policy.names.iter().any(|allowed| allowed == name)
        || policy
            .prefixes
            .iter()
            .any(|prefix| name.starts_with(prefix))
}

fn encoded_size(cookie: &Cookie<'_>) -> usize {
    struct Length(usize);
    impl std::fmt::Write for Length {
        fn write_str(&mut self, text: &str) -> std::fmt::Result {
            self.0 = self.0.saturating_add(text.len());
            Ok(())
        }
    }
    // Quota accounting uses the library's representation without allocating it.
    let mut length = Length(0);
    write!(&mut length, "{}", **cookie).expect("counting cookie bytes cannot fail");
    length.0
}

impl CookieJar {
    pub(crate) fn clear(&self) {
        self.store.lock().expect("cookie jar poisoned").clear();
    }

    /// Fill the guest's first empty Cookie slot, or remove empty slots. An
    /// explicit nonempty Cookie always takes precedence, regardless of order.
    pub(crate) fn apply(&self, policy: &CookiePolicy, url: &Url, fields: &mut Vec<Field>) {
        let explicit = fields
            .iter()
            .any(|field| field.name.eq_ignore_ascii_case("cookie") && !field.bytes.is_empty());
        if explicit {
            fields.retain(|field| {
                !field.name.eq_ignore_ascii_case("cookie") || !field.bytes.is_empty()
            });
            return;
        }
        if !fields
            .iter()
            .any(|field| field.name.eq_ignore_ascii_case("cookie") && field.bytes.is_empty())
        {
            return;
        }
        let value = matching_url(policy, url).and_then(|url| {
            let store = self.store.lock().expect("cookie jar poisoned");
            let mut value = String::new();
            for (name, cookie_value) in store.get_request_values(&url) {
                if allowed_name(policy, name) {
                    if !value.is_empty() {
                        value.push_str("; ");
                    }
                    value.push_str(name);
                    value.push('=');
                    value.push_str(cookie_value);
                }
            }
            (!value.is_empty()).then(|| value.into_bytes())
        });
        let mut value = value;
        fields.retain_mut(|field| {
            if field.name.eq_ignore_ascii_case("cookie") && field.bytes.is_empty() {
                if let Some(bytes) = value.take() {
                    field.bytes = bytes;
                    true
                } else {
                    false
                }
            } else {
                true
            }
        });
    }

    /// Apply one response atomically: a quota failure leaves the entire jar
    /// unchanged. Invalid Set-Cookie bytes and syntax are ignored by the same
    /// rules as cookie_store; no cookie content is logged or persisted.
    pub(crate) fn absorb<'a>(
        &self,
        policy: &CookiePolicy,
        url: &Url,
        headers: impl IntoIterator<Item = &'a [u8]>,
    ) -> Result<(), CookieLimit> {
        let mut headers = headers.into_iter().peekable();
        if headers.peek().is_none() {
            return Ok(());
        }
        let Some(url) = matching_url(policy, url) else {
            return Ok(());
        };
        let mut store = self.store.lock().expect("cookie jar poisoned");
        // Expired entries are never observable and must not occupy quota or
        // rollback space. cookie_store otherwise retains expired tombstones.
        let expired: Vec<_> = store
            .iter_any()
            .filter(|cookie| cookie.is_expired())
            .map(|cookie| {
                (
                    String::from(&cookie.domain),
                    String::from(&cookie.path),
                    cookie.name().to_owned(),
                )
            })
            .collect();
        for (domain, path, name) in expired {
            drop(store.remove(&domain, &path, &name));
        }

        type Key = (String, String, String);
        let mut undo: HashMap<Key, Option<Cookie<'static>>> = HashMap::new();
        let mut undo_bytes = 0usize;
        let mut count = 0usize;
        let mut size = 0usize;
        for cookie in store.iter_unexpired() {
            count += 1;
            size = size.saturating_add(encoded_size(cookie));
        }
        let mut limit = false;
        for bytes in headers {
            let Ok(text) = std::str::from_utf8(bytes) else {
                continue;
            };
            let Ok(cookie) = Cookie::parse(text, &url) else {
                continue;
            };
            if !allowed_name(policy, cookie.name()) {
                continue;
            }
            // Bound both the live intermediate store and undo journal. A
            // response may temporarily exceed the final quota (e.g. it adds
            // before deleting), but cannot consume unbounded memory.
            let key = (
                String::from(&cookie.domain),
                String::from(&cookie.path),
                cookie.name().to_owned(),
            );
            let key_bytes = key
                .0
                .len()
                .saturating_add(key.1.len())
                .saturating_add(key.2.len());
            if key_bytes > MAX_BYTES || text.len() > 2 * MAX_BYTES {
                limit = true;
                break;
            }
            let previous = store.get_any(&key.0, &key.1, &key.2);
            let had_old = previous.is_some();
            let old_size = previous.map_or(0, encoded_size);
            let old = if undo.contains_key(&key) {
                None
            } else {
                previous.cloned()
            };
            // `into_owned` would retain the entire received header, including
            // ignored attributes. Reparse the library's compact representation
            // instead, while preserving the already-derived RFC state (notably
            // the absolute Max-Age deadline and host-only/default-path flags).
            let mut raw = (*cookie).clone();
            // Display synthesizes Secure for Partitioned and SameSite=None.
            // Suppress those additions and restore Partitioned after parsing;
            // otherwise formatting can change the cookie's matching behavior.
            let partitioned = raw.partitioned();
            raw.set_partitioned(false);
            if raw.secure().is_none() {
                raw.set_secure(false);
            }
            let canonical = raw.to_string();
            let mut raw = cookie_store::RawCookie::parse(canonical.as_str())
                .expect("a formatted parsed cookie remains parseable");
            raw.set_partitioned(partitioned);
            let mut compact = Cookie::try_from_raw_cookie(&raw, &url)
                .expect("a formatted accepted cookie remains valid")
                .into_owned();
            compact.domain = cookie.domain;
            compact.path = cookie.path;
            compact.expires = cookie.expires;
            // Insertion rejection is cookie_store's normal invalid-cookie
            // handling; it must not create a journal entry.
            if store.insert(compact, &url).is_err() {
                continue;
            }
            let current = store.get_any(&key.0, &key.1, &key.2);
            let expired = current.is_some_and(|cookie| cookie.is_expired());
            let new_size = current
                .filter(|cookie| !cookie.is_expired())
                .map_or(0, encoded_size);
            if expired {
                drop(store.remove(&key.0, &key.1, &key.2));
            }
            count = count.saturating_sub(usize::from(had_old)) + usize::from(new_size != 0);
            size = size.saturating_sub(old_size).saturating_add(new_size);
            if let std::collections::hash_map::Entry::Vacant(entry) = undo.entry(key) {
                undo_bytes = undo_bytes
                    .saturating_add(key_bytes)
                    .saturating_add(old_size);
                entry.insert(old);
            }
            if count > 2 * MAX_COOKIES
                || size > 2 * MAX_BYTES
                || undo.len() > 2 * MAX_COOKIES
                || undo_bytes > 2 * MAX_BYTES
            {
                limit = true;
                break;
            }
        }
        // Cookies can expire during a response; final quota is based on the
        // live entries rather than the incremental upper bound above.
        count = 0;
        size = 0;
        for cookie in store.iter_unexpired() {
            count += 1;
            size = size.saturating_add(encoded_size(cookie));
        }
        if limit || count > MAX_COOKIES || size > MAX_BYTES {
            for ((domain, path, name), original) in undo {
                drop(store.remove(&domain, &path, &name));
                if let Some(cookie) = original {
                    // A Domain cookie from a subdomain can replace a
                    // host-only cookie at the same key. Restore using the
                    // original cookie's host, not necessarily this request's.
                    let restored = if cookie.domain.matches(&url) {
                        store.insert(cookie, &url)
                    } else {
                        let mut origin = url.as_ref().clone();
                        origin
                            .set_host(Some(&domain))
                            .expect("stored cookie domain");
                        store.insert(cookie, &origin)
                    };
                    match restored {
                        Ok(_) | Err(CookieError::Expired) => {}
                        Err(error) => {
                            panic!("previously accepted cookie cannot be restored: {error}")
                        }
                    }
                }
            }
            return Err(CookieLimit);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> CookiePolicy {
        CookiePolicy::parse(r#"{"domains":["example.com","sub.example.com"],"names":["sid"],"prefixes":["sticky_"]}"#).unwrap()
    }
    fn url(text: &str) -> Url {
        Url::parse(text).unwrap()
    }
    fn header(jar: &CookieJar, policy: &CookiePolicy, target: &str) -> Option<String> {
        let mut fields = vec![Field {
            name: "Cookie".into(),
            bytes: vec![],
        }];
        jar.apply(policy, &url(target), &mut fields);
        fields
            .first()
            .map(|field| String::from_utf8(field.bytes.clone()).unwrap())
    }
    fn add(
        jar: &CookieJar,
        policy: &CookiePolicy,
        target: &str,
        values: &[&str],
    ) -> Result<(), CookieLimit> {
        jar.absorb(
            policy,
            &url(target),
            values.iter().map(|value| value.as_bytes()),
        )
    }

    #[test]
    fn matches_domain_path_secure_expiration_and_policy() {
        let jar = CookieJar::default();
        let p = policy();
        add(
            &jar,
            &p,
            "https://example.com/a/start",
            &[
                "sid=one; Path=/a; Secure",
                "sticky_route=two; Domain=example.com; Path=/",
                "not_allowed=no; Path=/",
                "sticky_expired=gone; Max-Age=0",
                "invalid",
            ],
        )
        .unwrap();
        let selected = header(&jar, &p, "https://example.com/a/next").unwrap();
        assert!(selected.contains("sid=one"));
        assert!(selected.contains("sticky_route=two"));
        assert!(!selected.contains("not_allowed"));
        assert!(!selected.contains("sticky_expired"));
        assert_eq!(
            header(&jar, &p, "https://example.com/b"),
            Some("sticky_route=two".into())
        );
        assert_eq!(
            header(&jar, &p, "wss://sub.example.com/a"),
            Some("sticky_route=two".into())
        );
        assert_eq!(header(&jar, &p, "http://example.com/a"), None);
        jar.absorb(
            &p,
            &url("https://example.com/a"),
            [b"sid=\xff".as_slice(), b"not a cookie".as_slice()],
        )
        .unwrap();
        assert!(
            header(&jar, &p, "https://example.com/a")
                .unwrap()
                .contains("sid=one")
        );
        assert_eq!(header(&jar, &p, "https://other.example.com/a"), None);
        add(
            &jar,
            &p,
            "https://example.com/a",
            &["sid=delete; Path=/a; Max-Age=0"],
        )
        .unwrap();
        assert_eq!(
            header(&jar, &p, "https://example.com/a"),
            Some("sticky_route=two".into())
        );
    }

    #[test]
    fn policy_matches_only_explicit_hosts() {
        let jar = CookieJar::default();
        let p = CookiePolicy::parse(r#"{"domains":["example.com"],"names":["sid"]}"#).unwrap();
        add(
            &jar,
            &p,
            "https://example.com/",
            &["sid=root; Domain=example.com"],
        )
        .unwrap();
        add(
            &jar,
            &p,
            "https://sub.example.com/",
            &["sid=not-admitted; Domain=example.com"],
        )
        .unwrap();
        assert_eq!(
            header(&jar, &p, "https://example.com/"),
            Some("sid=root".into())
        );
        assert_eq!(header(&jar, &p, "https://sub.example.com/"), None);
        let both =
            CookiePolicy::parse(r#"{"domains":["example.com","sub.example.com"],"names":["sid"]}"#)
                .unwrap();
        assert_eq!(
            header(&jar, &both, "https://sub.example.com/"),
            Some("sid=root".into())
        );
        assert_eq!(header(&jar, &both, "https://notexample.com/"), None);
    }

    #[test]
    fn empty_slot_and_explicit_precedence() {
        let jar = CookieJar::default();
        let p = policy();
        let target = url("https://example.com/");
        assert_eq!(header(&jar, &p, target.as_str()), None);
        add(&jar, &p, target.as_str(), &["sid=jar"]).unwrap();
        let mut fields = vec![
            Field {
                name: "Cookie".into(),
                bytes: vec![],
            },
            Field {
                name: "cookie".into(),
                bytes: b"sid=guest".to_vec(),
            },
        ];
        jar.apply(&p, &target, &mut fields);
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].bytes, b"sid=guest");
        assert_eq!(header(&jar, &p, target.as_str()), Some("sid=jar".into()));
        assert_eq!(header(&CookieJar::default(), &p, target.as_str()), None);
    }

    #[tokio::test]
    async fn registry_isolates_providers_epochs_and_reset() {
        use crate::plugin::volatile_state::VolatileStore;
        let registry = VolatileStore::for_installation("cookie-test-isolation");
        let p = policy();
        let guard = registry.read().await;
        let first = guard.cookies("vendor", "one", 1, 1).unwrap();
        let second = guard.cookies("vendor", "two", 1, 1).unwrap();
        add(&first, &p, "https://example.com/", &["sid=secret"]).unwrap();
        assert_eq!(header(&second, &p, "https://example.com/"), None);
        let other_epoch = guard.cookies("vendor", "one", 2, 1).unwrap();
        assert_eq!(header(&first, &p, "https://example.com/"), None);
        assert_eq!(header(&other_epoch, &p, "https://example.com/"), None);
        assert!(guard.cookies("vendor", "one", 1, 1).is_err());
        add(&other_epoch, &p, "https://example.com/", &["sid=epoch"]).unwrap();
        let other_fence = guard.cookies("vendor", "one", 1, 2).unwrap();
        assert_eq!(header(&other_epoch, &p, "https://example.com/"), None);
        assert_eq!(header(&other_fence, &p, "https://example.com/"), None);
        assert!(guard.cookies("vendor", "one", 2, 1).is_err());
        add(&other_fence, &p, "https://example.com/", &["sid=fence"]).unwrap();
        drop(guard);
        let lifecycle = registry.lock().await;
        lifecycle.invalidate_provider("vendor", "one").await;
        drop(lifecycle);
        assert_eq!(header(&other_fence, &p, "https://example.com/"), None);
        let guard = registry.read().await;
        let reset = guard.cookies("vendor", "one", 1, 3).unwrap();
        assert_eq!(header(&reset, &p, "https://example.com/"), None);
    }

    #[test]
    fn byte_quota_rejects_update_without_changing_existing_cookies() {
        let jar = CookieJar::default();
        let p = policy();
        let target = "https://example.com/";
        add(&jar, &p, target, &["sid=old"]).unwrap();
        let huge = format!("sticky_large={}", "x".repeat(MAX_BYTES));
        assert_eq!(add(&jar, &p, target, &["sid=new", &huge]), Err(CookieLimit));
        assert_eq!(header(&jar, &p, target), Some("sid=old".into()));
    }

    #[test]
    fn ignored_attributes_are_not_retained_across_updates_or_rollback() {
        let jar = CookieJar::default();
        let p = policy();
        let target = "https://example.com/a/start";
        let padding = "ignored-padding-marker".repeat(MAX_BYTES / 16);
        for n in 0..4 {
            let value =
                format!("sticky_{n}=value; Path=/a; SameSite=None; Partitioned; Ignored={padding}");
            add(&jar, &p, target, &[&value]).unwrap();
        }
        {
            let store = jar.store.lock().unwrap();
            let cookies: Vec<_> = store.iter_unexpired().collect();
            assert_eq!(cookies.len(), 4);
            for cookie in cookies {
                // Debug includes cookie's owned backing string, not merely its
                // formatted attributes. This detects the original retention bug.
                assert!(!format!("{cookie:?}").contains("ignored-padding-marker"));
                assert_eq!(cookie.secure(), None);
                assert_eq!(cookie.partitioned(), Some(true));
            }
        }
        assert_eq!(header(&jar, &p, "https://sub.example.com/a"), None);
        assert_eq!(header(&jar, &p, "https://example.com/b"), None);
        let huge = format!("sticky_large={}", "x".repeat(MAX_BYTES));
        assert_eq!(
            add(&jar, &p, target, &["sticky_0=changed; Path=/a", &huge]),
            Err(CookieLimit)
        );
        let selected = header(&jar, &p, target).unwrap();
        assert!(selected.contains("sticky_0=value"));
        assert!(!selected.contains("sticky_0=changed"));
        let store = jar.store.lock().unwrap();
        assert!(
            store
                .iter_unexpired()
                .all(|cookie| !format!("{cookie:?}").contains("ignored-padding-marker"))
        );
    }

    #[test]
    fn repeated_updates_and_deletions_apply_in_order() {
        let jar = CookieJar::default();
        let p = policy();
        let target = "https://example.com/";
        add(
            &jar,
            &p,
            target,
            &["sid=one", "sid=two", "sid=gone; Max-Age=0", "sid=last"],
        )
        .unwrap();
        assert_eq!(header(&jar, &p, target), Some("sid=last".into()));
        add(&jar, &p, target, &["sid=next", "sid=gone; Max-Age=0"]).unwrap();
        assert_eq!(header(&jar, &p, target), None);
    }

    #[test]
    fn rollback_restores_host_only_cookie_replaced_from_subdomain() {
        let jar = CookieJar::default();
        let p = policy();
        add(&jar, &p, "https://example.com/", &["sid=host-only; Path=/"]).unwrap();
        let huge = format!("sticky_large={}", "x".repeat(MAX_BYTES));
        assert_eq!(
            add(
                &jar,
                &p,
                "https://sub.example.com/",
                &["sid=domain; Domain=example.com; Path=/", &huge,]
            ),
            Err(CookieLimit)
        );
        assert_eq!(
            header(&jar, &p, "https://example.com/"),
            Some("sid=host-only".into())
        );
        assert_eq!(header(&jar, &p, "https://sub.example.com/"), None);
    }

    #[test]
    fn mutation_journal_is_bounded_and_rolls_back() {
        let jar = CookieJar::default();
        let p = policy();
        let target = "https://example.com/";
        add(&jar, &p, target, &["sid=original"]).unwrap();
        let mut updates = vec!["sid=changed".to_owned()];
        for n in 0..=2 * MAX_COOKIES {
            updates.push(format!("sticky_{n}=x"));
            updates.push(format!("sticky_{n}=x; Max-Age=0"));
        }
        assert_eq!(
            jar.absorb(&p, &url(target), updates.iter().map(|s| s.as_bytes())),
            Err(CookieLimit)
        );
        assert_eq!(header(&jar, &p, target), Some("sid=original".into()));
    }

    #[test]
    fn quota_failure_is_atomic_and_deletion_frees_capacity() {
        let jar = CookieJar::default();
        let p = policy();
        let target = "https://example.com/";
        for n in 0..MAX_COOKIES {
            add(&jar, &p, target, &[&format!("sticky_{n}=x; Path=/")]).unwrap();
        }
        assert_eq!(
            add(
                &jar,
                &p,
                target,
                &[
                    "sticky_0=changed; Path=/",
                    "sticky_0=changed-again; Path=/",
                    "sticky_overflow=x; Path=/"
                ]
            ),
            Err(CookieLimit)
        );
        let result = header(&jar, &p, target).unwrap();
        assert!(result.contains("sticky_0=x"));
        assert!(!result.contains("sticky_overflow"));
        add(
            &jar,
            &p,
            target,
            &["sticky_0=x; Path=/; Max-Age=0", "sticky_new=x; Path=/"],
        )
        .unwrap();
        assert!(header(&jar, &p, target).unwrap().contains("sticky_new=x"));
    }
}
