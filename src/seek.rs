// githttp-fs
//
// Git-based Content Management System
// Copyright: 2026, Valerian Saliou <valerian@valeriansaliou.name>
// License: Mozilla Public License v2.0 (MPL v2.0)

//! Line-based content windowing ("seek") for file read endpoints.
//!
//! Three types split the wire formats from the scan:
//!
//! - [`SeekOptions`] deserialises straight from `seek_`-prefixed HTTP query
//!   parameters on the single-file read route. Prefix lists travel as
//!   JSON-array *strings* (query parameters are strings — one canonical
//!   spelling, no polymorphism for client libraries).
//! - [`SeekBody`] is the JSON-body twin used by the batch read route:
//!   native arrays and no `seek_` prefix on the field names, since they
//!   already nest under a `"seek"` object.
//! - [`SeekFilter`] is the parsed, trusted form that the git layer feeds
//!   with content. Both wire types funnel through [`SeekFilter::build`],
//!   so every malformed value becomes a `400` with identical rules.
//!
//! The scan is a single forward pass over an [`std::io::BufRead`], so the
//! git layer can feed it either a streaming ODB reader (stops inflating a
//! loose object as soon as the window is complete) or an in-memory blob
//! wrapped in a `Cursor` (packed objects — libgit2 cannot stream those).
//! Only the selected window is ever allocated; the full file content is
//! never copied.

use std::io::BufRead;

use serde::Deserialize;

use crate::error::AppError;

/// Meta value usable in `seek_to_line_starts_with` (bare, or inside the
/// array's elements): every occurrence is replaced, once the window opens,
/// by whichever `seek_from_line_starts_with` prefix actually matched. This
/// lets a multi-prefix seek stop on the *same* marker it started on (e.g.
/// from `["## ", "### "]` to `$seek_from_line_starts_with` selects a
/// section up to and including the next heading of the same level).
pub const SEEK_TO_FROM_META: &str = "$seek_from_line_starts_with";

/// How the seek fields are named on a given wire format, so validation
/// errors point at the exact parameter the caller sent.
struct SeekParameterNames {
    from: &'static str,
    to: &'static str,
    maximum: &'static str,
}

const QUERY_PARAMETER_NAMES: SeekParameterNames = SeekParameterNames {
    from: "seek_from_line_starts_with",
    to: "seek_to_line_starts_with",
    maximum: "seek_lines_maximum",
};

const BODY_PARAMETER_NAMES: SeekParameterNames = SeekParameterNames {
    from: "seek.from_line_starts_with",
    to: "seek.to_line_starts_with",
    maximum: "seek.lines_maximum",
};

/// The seek filters exactly as they arrive on the wire, before any
/// validation. All fields are optional and combinable; an empty struct is
/// the identity. [`parse`](Self::parse) turns them into a usable
/// [`SeekFilter`] and rejects every malformed spelling with a `400`:
///
/// - `seek_from_line_starts_with` — a JSON array of non-empty strings
///   (e.g. `["---", "+++"]`, URL-encoded in a query string).
/// - `seek_to_line_starts_with` — the same JSON array format, or the bare
///   meta value [`SEEK_TO_FROM_META`] as a shorthand for `[that value]`.
/// - `seek_lines_maximum` — a positive integer.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SeekOptions {
    pub seek_from_line_starts_with: Option<String>,
    pub seek_to_line_starts_with: Option<String>,
    pub seek_lines_maximum: Option<usize>,
}

impl SeekOptions {
    /// Decodes and validates the raw filters. Every rejection is an
    /// `InvalidOperation` (HTTP `400`): non-JSON-array values, empty
    /// arrays, empty prefixes, a zero maximum, and a
    /// `$seek_from_line_starts_with` meta value with no `from` filter to
    /// resolve it.
    pub fn parse(&self) -> Result<SeekFilter, AppError> {
        let names = &QUERY_PARAMETER_NAMES;

        let from_prefixes = self
            .seek_from_line_starts_with
            .as_deref()
            .map(|raw| Self::parse_prefix_array(raw, names.from))
            .transpose()?;

        let to_prefixes = match self.seek_to_line_starts_with.as_deref() {
            None => None,

            // Bare meta value: shorthand for an array holding only the meta.
            Some(SEEK_TO_FROM_META) => Some(vec![SEEK_TO_FROM_META.to_string()]),

            Some(raw) => Some(Self::parse_prefix_array(raw, names.to)?),
        };

        SeekFilter::build(from_prefixes, to_prefixes, self.seek_lines_maximum, names)
    }

    /// Decodes one prefix-list value from its JSON-array-string spelling.
    fn parse_prefix_array(raw: &str, parameter: &str) -> Result<Vec<String>, AppError> {
        serde_json::from_str(raw).map_err(|_err| AppError::InvalidOperation {
            reason: format!(
                "{} must be a JSON array of strings, e.g. [\"## \"]",
                parameter
            ),
        })
    }
}

/// The seek filters as they appear in JSON request bodies (batch read
/// route), nested under a `"seek"` object: native arrays, no `seek_`
/// prefix on the field names. Same semantics and validation rules as
/// [`SeekOptions`] — [`parse`](Self::parse) applies them and rejects
/// malformed values with a `400`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SeekBody {
    pub from_line_starts_with: Option<Vec<String>>,
    pub to_line_starts_with: Option<SeekBodyToPrefixes>,
    pub lines_maximum: Option<usize>,
}

/// The two accepted spellings of `seek.to_line_starts_with` in a JSON body,
/// mirroring the query parameter exactly: prefix values must be an array,
/// and the only bare string allowed is the [`SEEK_TO_FROM_META`] operator
/// (shorthand for an array holding only it) — so clients passing the
/// operator do not have to wrap it. Any other bare string is a `400`,
/// enforced in [`SeekBody::parse`] (serde's untagged repartition alone
/// cannot tell the operator from an arbitrary string).
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum SeekBodyToPrefixes {
    Value(String),
    Values(Vec<String>),
}

impl SeekBody {
    /// Validates the body filters into a usable [`SeekFilter`]. Arrays are
    /// already native here, so unlike [`SeekOptions::parse`] there is no
    /// JSON-string decoding step.
    pub fn parse(&self) -> Result<SeekFilter, AppError> {
        let names = &BODY_PARAMETER_NAMES;

        let to_prefixes = match &self.to_line_starts_with {
            None => None,

            // Bare meta value: shorthand for an array holding only the meta.
            Some(SeekBodyToPrefixes::Value(value)) if value == SEEK_TO_FROM_META => {
                Some(vec![SEEK_TO_FROM_META.to_string()])
            }

            Some(SeekBodyToPrefixes::Value(_)) => {
                return Err(AppError::InvalidOperation {
                    reason: format!(
                        "{} must be a JSON array of strings, or the bare meta value {}",
                        names.to, SEEK_TO_FROM_META
                    ),
                });
            }

            Some(SeekBodyToPrefixes::Values(values)) => Some(values.clone()),
        };

        SeekFilter::build(
            self.from_line_starts_with.clone(),
            to_prefixes,
            self.lines_maximum,
            names,
        )
    }
}

/// Parsed line-based window, applied to file content as it is read.
///
/// The window is resolved in three steps, each narrowing the previous one:
///
/// 1. `from_prefixes` — the window starts at the first line whose text
///    starts with *any* of the prefixes (that line included; on a line
///    matching several, the first prefix in the given order wins and is
///    what the `$seek_from_line_starts_with` meta resolves to). When no
///    line matches, the window is empty. Omitted: the window starts at
///    line 0.
/// 2. `to_prefixes` — the window stops *at* the first line whose text
///    starts with any of the prefixes, that line included as the window's
///    last line. The search begins on the line *after* the window's first
///    line, so the window always contains at least its first line — which
///    is what lets the same prefix be used for both bounds (e.g. from
///    `---` to `---` selects a whole front-matter block, both markers
///    included). Occurrences of [`SEEK_TO_FROM_META`] in the prefixes are
///    replaced by the matched `from` prefix. When no line matches, the
///    window runs to the end of the file.
/// 3. `lines_maximum` — the window is capped to this many lines, counted
///    from the window's first line.
#[derive(Debug, Clone, Default)]
pub struct SeekFilter {
    pub from_prefixes: Option<Vec<String>>,
    pub to_prefixes: Option<Vec<String>>,
    pub lines_maximum: Option<usize>,
}

impl SeekFilter {
    /// Validates decoded filter values into a trusted `SeekFilter` — the
    /// single funnel behind every wire format. Rejections are
    /// `InvalidOperation` (HTTP `400`), naming the parameter as the caller
    /// spelled it: empty arrays and empty prefixes (an empty prefix would
    /// match every line, an empty array none — both can only be caller
    /// bugs), a zero maximum, and a `$seek_from_line_starts_with` meta
    /// value with no `from` filter to resolve it.
    fn build(
        from_prefixes: Option<Vec<String>>,
        to_prefixes: Option<Vec<String>>,
        lines_maximum: Option<usize>,
        names: &SeekParameterNames,
    ) -> Result<Self, AppError> {
        for (prefixes, parameter) in [(&from_prefixes, names.from), (&to_prefixes, names.to)] {
            let Some(prefixes) = prefixes else { continue };

            if prefixes.is_empty() {
                return Err(AppError::InvalidOperation {
                    reason: format!("{} must contain at least one prefix", parameter),
                });
            }

            if prefixes.iter().any(|prefix| prefix.is_empty()) {
                return Err(AppError::InvalidOperation {
                    reason: format!("{} prefixes must not be empty", parameter),
                });
            }
        }

        if from_prefixes.is_none() {
            if let Some(to_prefixes) = &to_prefixes {
                if to_prefixes
                    .iter()
                    .any(|prefix| prefix.contains(SEEK_TO_FROM_META))
                {
                    return Err(AppError::InvalidOperation {
                        reason: format!(
                            "{} uses {} but {} is not set",
                            names.to, SEEK_TO_FROM_META, names.from
                        ),
                    });
                }
            }
        }

        if lines_maximum == Some(0) {
            return Err(AppError::InvalidOperation {
                reason: format!("{} must be at least 1", names.maximum),
            });
        }

        Ok(Self {
            from_prefixes,
            to_prefixes,
            lines_maximum,
        })
    }

    /// True when no filter is set, i.e. the scan would return the content
    /// untouched. Lets callers keep the plain whole-blob read path.
    pub fn is_noop(&self) -> bool {
        self.from_prefixes.is_none() && self.to_prefixes.is_none() && self.lines_maximum.is_none()
    }

    /// Scans `reader` line by line and returns the configured window (see
    /// the struct docs for the exact resolution order). The scan stops
    /// reading as soon as the window is known to be complete (end marker
    /// taken or line cap reached), which is what makes streaming sources
    /// worthwhile: the remainder of the object is never even inflated.
    ///
    /// Matching happens on raw bytes (a UTF-8 prefix match is exact at the
    /// byte level, and a multi-byte character can never contain `\n`), so
    /// only the returned *window* is required to be valid UTF-8 — invalid
    /// bytes outside it are never decoded. Lines keep their terminators, so
    /// the window is returned byte-for-byte: CRLF endings and the presence
    /// or absence of a final newline survive the round-trip.
    pub fn apply_reader<R: BufRead>(
        &self,
        mut reader: R,
        file_path: &str,
    ) -> Result<String, AppError> {
        let from_prefixes = self.from_prefixes.as_deref();

        // Without a `from` filter the window is already open at line 0 and
        // the end markers can be resolved upfront (parse() guarantees they
        // hold no meta value in that case). With one, they are resolved
        // when the window opens, from the prefix that matched.
        let mut in_window = from_prefixes.is_none();
        let mut to_prefixes: Option<Vec<String>> = match from_prefixes {
            None => self.to_prefixes.clone(),
            Some(_) => None,
        };

        let mut window: Vec<u8> = Vec::new();
        let mut lines_taken: usize = 0;
        let mut line: Vec<u8> = Vec::new();
        let mut window_closed = false;

        loop {
            line.clear();

            if reader.read_until(b'\n', &mut line)? == 0 {
                break;
            }

            if !in_window {
                let Some(matched) = from_prefixes
                    .unwrap_or_default()
                    .iter()
                    .find(|prefix| line.starts_with(prefix.as_bytes()))
                else {
                    continue;
                };

                in_window = true;

                to_prefixes = self.to_prefixes.as_ref().map(|prefixes| {
                    prefixes
                        .iter()
                        .map(|prefix| prefix.replace(SEEK_TO_FROM_META, matched))
                        .collect()
                });
            } else if lines_taken > 0 {
                // The end markers are never checked against the window's
                // first line (`lines_taken == 0` covers the from-less case;
                // a matched `from` line skips this branch entirely), so the
                // window always contains at least its first line. The
                // matched line itself is *included*: it closes the window
                // after being taken.
                if let Some(prefixes) = &to_prefixes {
                    if prefixes
                        .iter()
                        .any(|prefix| line.starts_with(prefix.as_bytes()))
                    {
                        window_closed = true;
                    }
                }
            }

            window.extend_from_slice(&line);
            lines_taken += 1;

            if window_closed || self.lines_maximum == Some(lines_taken) {
                break;
            }
        }

        String::from_utf8(window).map_err(|_err| AppError::InvalidUtf8 {
            path: file_path.to_string(),
        })
    }
}
