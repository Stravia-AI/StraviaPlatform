//! 搜索工作区按同时进行的搜索借还，不随扫描线程数永久复制。
//! 编译语义与 regex::bytes 相同；锁只保护空闲工作区，不串行化匹配。
use std::sync::Arc;

use parking_lot::Mutex;
use regex_automata::{
    Input, MatchKind, PatternID, meta,
    util::{captures, iter::Searcher, syntax},
};

#[derive(Clone)]
pub(super) struct Regex(Arc<Compiled>);

struct Compiled {
    engine: meta::Regex,
    idle: Mutex<Vec<Scratch>>,
}

struct Scratch {
    cache: meta::Cache,
    captures: captures::Captures,
}

struct Lease<'r> {
    regex: &'r Regex,
    scratch: Option<Scratch>,
}

impl Drop for Lease<'_> {
    fn drop(&mut self) {
        self.regex
            .0
            .idle
            .lock()
            .push(self.scratch.take().expect("owned search workspace"));
    }
}

impl Regex {
    #[cfg(test)]
    pub(super) fn new(pattern: &str) -> Result<Self, Box<meta::BuildError>> {
        Self::with_options(pattern, true, 10 * 1024 * 1024)
    }

    pub(super) fn with_options(
        pattern: &str,
        unicode: bool,
        size_limit: usize,
    ) -> Result<Self, Box<meta::BuildError>> {
        let engine = meta::Regex::builder()
            .configure(
                meta::Config::new()
                    .match_kind(MatchKind::LeftmostFirst)
                    .utf8_empty(false)
                    .nfa_size_limit(Some(size_limit))
                    .hybrid_cache_capacity(2 * 1024 * 1024),
            )
            .syntax(syntax::Config::new().utf8(false).unicode(unicode))
            .build(pattern)
            .map_err(Box::new)?;
        Ok(Self(Arc::new(Compiled {
            engine,
            idle: Mutex::new(Vec::new()),
        })))
    }

    fn lease(&self) -> Lease<'_> {
        let available = self.0.idle.lock().pop();
        let scratch = available.unwrap_or_else(|| Scratch {
            cache: self.0.engine.create_cache(),
            captures: self.0.engine.create_captures(),
        });
        Lease {
            regex: self,
            scratch: Some(scratch),
        }
    }

    pub(super) fn captures_len(&self) -> usize {
        self.0.engine.captures_len()
    }

    pub(super) fn capture_names(&self) -> impl Iterator<Item = Option<&str>> {
        self.0.engine.group_info().pattern_names(PatternID::ZERO)
    }

    pub(super) fn is_match(&self, haystack: &[u8]) -> bool {
        let mut lease = self.lease();
        self.0
            .engine
            .search_half_with(
                &mut lease.scratch.as_mut().unwrap().cache,
                &Input::new(haystack).earliest(true),
            )
            .is_some()
    }

    pub(super) fn find<'h>(&self, haystack: &'h [u8]) -> Option<Match<'h>> {
        let mut lease = self.lease();
        self.0
            .engine
            .search_with(
                &mut lease.scratch.as_mut().unwrap().cache,
                &Input::new(haystack),
            )
            .map(|found| Match {
                haystack,
                start: found.start(),
                end: found.end(),
            })
    }

    pub(super) fn captures<'h>(&self, haystack: &'h [u8]) -> Option<Captures<'h>> {
        let mut lease = self.lease();
        let scratch = lease.scratch.as_mut().unwrap();
        self.0.engine.search_captures_with(
            &mut scratch.cache,
            &Input::new(haystack),
            &mut scratch.captures,
        );
        scratch.captures.is_match().then(|| Captures {
            haystack,
            captures: scratch.captures.clone(),
        })
    }

    pub(super) fn find_iter<'r, 'h>(
        &'r self,
        haystack: &'h [u8],
    ) -> impl Iterator<Item = Match<'h>> + 'r + use<'r, 'h>
    where
        'h: 'r,
    {
        let mut lease = self.lease();
        let mut searcher = Searcher::new(Input::new(haystack));
        std::iter::from_fn(move || {
            searcher
                .advance(|input| {
                    Ok(self
                        .0
                        .engine
                        .search_with(&mut lease.scratch.as_mut().unwrap().cache, input))
                })
                .map(|found| Match {
                    haystack,
                    start: found.start(),
                    end: found.end(),
                })
        })
    }

    pub(super) fn captures_iter<'r, 'h>(
        &'r self,
        haystack: &'h [u8],
    ) -> impl Iterator<Item = Captures<'h>> + 'r + use<'r, 'h>
    where
        'h: 'r,
    {
        let mut lease = self.lease();
        let mut searcher = Searcher::new(Input::new(haystack));
        std::iter::from_fn(move || {
            let scratch = lease.scratch.as_mut().unwrap();
            searcher.advance(|input| {
                self.0.engine.search_captures_with(
                    &mut scratch.cache,
                    input,
                    &mut scratch.captures,
                );
                Ok(scratch.captures.get_match())
            })?;
            Some(Captures {
                haystack,
                captures: scratch.captures.clone(),
            })
        })
    }
}

pub(super) struct Captures<'h> {
    haystack: &'h [u8],
    captures: captures::Captures,
}

impl<'h> Captures<'h> {
    pub(super) fn len(&self) -> usize {
        self.captures.group_len()
    }
    pub(super) fn get(&self, index: usize) -> Option<Match<'h>> {
        self.captures.get_group(index).map(|span| Match {
            haystack: self.haystack,
            start: span.start,
            end: span.end,
        })
    }
    pub(super) fn name(&self, name: &str) -> Option<Match<'h>> {
        self.captures.get_group_by_name(name).map(|span| Match {
            haystack: self.haystack,
            start: span.start,
            end: span.end,
        })
    }
}

#[derive(Clone, Copy)]
pub(super) struct Match<'h> {
    haystack: &'h [u8],
    start: usize,
    end: usize,
}

impl<'h> Match<'h> {
    pub(super) fn start(self) -> usize {
        self.start
    }
    pub(super) fn end(self) -> usize {
        self.end
    }
    pub(super) fn is_empty(self) -> bool {
        self.start == self.end
    }
    pub(super) fn as_bytes(self) -> &'h [u8] {
        &self.haystack[self.start..self.end]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_reuse_preserves_byte_boundaries_empty_matches_and_captures() {
        let patterns = [
            "",
            "(?m)^",
            "a*",
            "a|",
            "(?:ab|a)+",
            "(?-u:.)",
            r"(?P<first>a+)?(?P<second>b*)",
            r"(?u:\b\w+\b)",
            r"(?P<TOKEN>[A-Z0-9]+)|(?P<fallback>other)",
        ];
        let inputs: &[&[u8]] = &[
            b"",
            b"aababa\nother",
            b"A7B9 other",
            "汉字 é\na".as_bytes(),
            b"\xffa\x80",
        ];
        for pattern in patterns {
            let regex = Regex::new(pattern).unwrap();
            let reference = regex::bytes::Regex::new(pattern).unwrap();
            std::thread::scope(|scope| {
                for rotation in 0..8 {
                    let regex = &regex;
                    let reference = &reference;
                    scope.spawn(move || {
                        for index in 0..inputs.len() * 3 {
                            let input = inputs[(index + rotation) % inputs.len()];
                            assert_eq!(regex.is_match(input), reference.is_match(input));
                            assert_eq!(
                                regex
                                    .find_iter(input)
                                    .map(|m| (m.start(), m.end()))
                                    .collect::<Vec<_>>(),
                                reference
                                    .find_iter(input)
                                    .map(|m| (m.start(), m.end()))
                                    .collect::<Vec<_>>()
                            );
                            let observed: Vec<_> = regex
                                .captures_iter(input)
                                .map(|captures| {
                                    (0..captures.len())
                                        .map(|i| captures.get(i).map(|m| (m.start(), m.end())))
                                        .collect::<Vec<_>>()
                                })
                                .collect();
                            let expected: Vec<_> = reference
                                .captures_iter(input)
                                .map(|captures| {
                                    (0..captures.len())
                                        .map(|i| captures.get(i).map(|m| (m.start(), m.end())))
                                        .collect::<Vec<_>>()
                                })
                                .collect();
                            assert_eq!(observed, expected);
                        }
                    });
                }
            });
        }
    }
}
