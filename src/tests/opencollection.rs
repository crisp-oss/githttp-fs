// githttp-fs
//
// Git-based Content Management System
// Copyright: 2026, Valerian Saliou <valerian@valeriansaliou.name>
// License: Mozilla Public License v2.0 (MPL v2.0)

//! Keeps the OpenCollection in `dev/opencollection/` in step with the API.
//!
//! CLAUDE.md requires every change to the API surface to be mirrored in the
//! collection in the same change. This test turns that rule into a failure
//! instead of a habit: it reads the routes `build_router` registers and the
//! request structs their handlers deserialise straight from the source, reads
//! every request file of the collection, and checks both directions — a route
//! with no request, a request for a route that no longer exists, a query
//! parameter or body key present on one side only — together with the
//! collection's own conventions for optional values: an optional query
//! parameter is `disabled: true`, an optional body key is a `//` comment line,
//! and a required one is neither.
//!
//! It parses source text rather than asking the router, because axum does not
//! expose its route table. The parser understands exactly the shapes this
//! codebase uses and panics on anything else (a `#[serde(rename)]`, a
//! `flatten`, a field split over two lines), so a refactor that outgrows it
//! fails here loudly instead of passing a check it can no longer make.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;
use yaml_rust2::{Yaml, YamlLoader};

const COLLECTION: &str = "dev/opencollection/collections/githttp-fs-server-api";
const ENDPOINT: &str = "{{endpoint}}";

/// Routes deliberately left out of the collection. The repository root's
/// order index has routes of its own only because axum's `{*path}` wildcard
/// needs at least one segment; they share their implementation with the
/// `/order/{*path}` routes, so a second set of requests would only duplicate
/// those. Each entry is still checked to exist, so a stale one fails too.
const NOT_IN_COLLECTION: &[(&str, &str)] = &[
    ("GET", "/{collection_id}/{tenant_id}/order"),
    ("PUT", "/{collection_id}/{tenant_id}/order"),
    ("DELETE", "/{collection_id}/{tenant_id}/order"),
];

/// A request struct's wire fields, each mapped to whether a caller may leave
/// it out (`Option<…>` or `#[serde(default)]`).
type Fields = BTreeMap<String, bool>;

/// One operation the API serves: a method on a path pattern, and what it reads.
struct Operation {
    method: String,
    path: String,
    query: Fields,
    body: Option<Fields>,
}

/// What one request file of the collection sends.
struct Request {
    file: String,
    method: String,
    path: String,
    url_query: Vec<String>,
    /// Query parameters, each with whether it is `disabled`.
    params: Vec<(String, bool)>,
    /// Top-level body keys: the live ones, and the `//`-commented ones.
    body: Option<(BTreeSet<String>, BTreeSet<String>)>,
}

fn root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|err| panic!("cannot read {}: {}", path.display(), err))
}

/// Drops whole-line `//` comments, so the bracket matching below never counts
/// a bracket written in prose.
fn strip_comments(source: &str) -> String {
    source
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The index of the bracket closing the one at `open`, skipping string
/// literals (route paths are full of braces).
fn closing(source: &str, open: usize) -> usize {
    let bytes = source.as_bytes();

    let (open_byte, close_byte) = match bytes[open] {
        b'(' => (b'(', b')'),
        b'{' => (b'{', b'}'),
        other => panic!("not an opening bracket: {}", other as char),
    };

    let mut depth = 0;
    let mut index = open;

    while index < bytes.len() {
        match bytes[index] {
            b'"' => {
                index += 1;

                while bytes[index] != b'"' {
                    if bytes[index] == b'\\' {
                        index += 1;
                    }

                    index += 1;
                }
            }
            byte if byte == open_byte => depth += 1,
            byte if byte == close_byte => {
                depth -= 1;

                if depth == 0 {
                    return index;
                }
            }
            _ => {}
        }

        index += 1;
    }

    panic!("unbalanced bracket at byte {}", open)
}

/// A function's parameter list and body, found by name.
fn function<'a>(source: &'a str, name: &str) -> (&'a str, &'a str) {
    let needle = format!("fn {}(", name);

    let params_open = source
        .find(&needle)
        .unwrap_or_else(|| panic!("no fn {}", name))
        + needle.len()
        - 1;
    let params_close = closing(source, params_open);

    let body_open = params_close
        + source[params_close..]
            .find('{')
            .unwrap_or_else(|| panic!("fn {} has no body", name));
    let body_close = closing(source, body_open);

    (
        &source[params_open + 1..params_close],
        &source[body_open + 1..body_close],
    )
}

/// The contents of the first string literal in `text`.
fn string_literal(text: &str) -> &str {
    let start = text.find('"').expect("no string literal") + 1;
    let end = start
        + text[start..]
            .find('"')
            .expect("unterminated string literal");

    &text[start..end]
}

/// The type an extractor wraps in a parameter list, e.g. `ListFilesQuery` for
/// `Query(query): Query<ListFilesQuery>`.
fn extractor<'a>(params: &'a str, wrapper: &str) -> Option<&'a str> {
    let needle = format!("{}<", wrapper);

    params.find(&needle).map(|index| {
        let start = index + needle.len();

        &params[start..start + params[start..].find('>').unwrap()]
    })
}

/// Every Rust source file outside the test suite, concatenated, for looking a
/// type up by name wherever it is declared.
fn sources() -> String {
    let mut all = String::new();
    let mut directories = vec![root().join("src")];

    while let Some(directory) = directories.pop() {
        for entry in fs::read_dir(&directory).unwrap() {
            let path = entry.unwrap().path();

            if path.is_dir() {
                if path.file_name().unwrap() != "tests" {
                    directories.push(path);
                }
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                all.push_str(&strip_comments(&read(&path)));
                all.push('\n');
            }
        }
    }

    all
}

/// The wire fields of a request struct, following a `type` alias to it.
fn fields(sources: &str, name: &str) -> Fields {
    let alias = format!("type {} = ", name);

    if let Some(index) = sources.find(&alias) {
        let target = &sources[index + alias.len()..];

        return fields(sources, target[..target.find(';').unwrap()].trim());
    }

    let needle = format!("struct {} {{", name);
    let start = sources
        .find(&needle)
        .unwrap_or_else(|| panic!("no struct {}", name));

    // The attributes between the previous item and this struct.
    let preamble = &sources[sources[..start].rfind('}').unwrap_or(0)..start];

    assert!(
        !preamble.contains("rename_all"),
        "struct {} renames its fields, which this test does not follow",
        name
    );

    let open = start + needle.len() - 1;
    let mut fields = Fields::new();
    let mut defaulted = false;

    for line in sources[open + 1..closing(sources, open)]
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        if line.starts_with("#[serde(") {
            for unsupported in ["rename", "alias", "flatten"] {
                assert!(
                    !line.contains(unsupported),
                    "struct {} uses `{}`, which this test does not follow: {}",
                    name,
                    unsupported,
                    line
                );
            }

            defaulted |= line.contains("default");

            continue;
        }

        if line.starts_with("#[") {
            continue;
        }

        let line = line
            .trim_start_matches("pub(crate) ")
            .trim_start_matches("pub ");
        let (field, field_type) = line
            .split_once(':')
            .unwrap_or_else(|| panic!("unparsed field in struct {}: {}", name, line));

        fields.insert(
            field.trim().to_string(),
            defaulted || field_type.trim().starts_with("Option<"),
        );

        defaulted = false;
    }

    fields
}

/// The `(method, handler)` pairs of one `.route(…)` call's method chain,
/// e.g. `get(routes::files::read_file).head(routes::files::file_exists)`.
fn handlers(chain: &str) -> Vec<(String, &str)> {
    let mut found = Vec::new();

    for method in ["get", "head", "put", "post", "delete", "patch"] {
        let needle = format!("{}(", method);

        for (index, _) in chain.match_indices(&needle) {
            let at_boundary = chain[..index]
                .chars()
                .last()
                .is_none_or(|before| !(before.is_alphanumeric() || before == '_'));

            if at_boundary {
                let rest = &chain[index + needle.len()..];

                found.push((
                    method.to_uppercase(),
                    rest[..rest.find(')').unwrap()].trim(),
                ));
            }
        }
    }

    if let Some(index) = chain.find("on(MethodFilter::") {
        let (method, rest) = chain[index + "on(MethodFilter::".len()..]
            .split_once(',')
            .unwrap();

        found.push((
            method.trim().to_string(),
            rest[..rest.find(')').unwrap()].trim(),
        ));
    }

    assert!(
        !found.is_empty(),
        "no handler recognised in route: {}",
        chain
    );

    found
}

/// The operations one handler serves on `path`. A handler taking a raw
/// `Json<Value>` dispatches on a path suffix (`/move`, `/reorder`), so each
/// suffix it strips is an operation of its own, reading the request struct of
/// the function it hands the body to.
fn expand(sources: &str, method: &str, path: &str, handler: &str) -> Vec<Operation> {
    let mut segments: Vec<&str> = handler.split("::").collect();
    let name = segments.pop().unwrap();
    let source = strip_comments(&read(
        &root().join(format!("src/{}.rs", segments.join("/"))),
    ));

    let (params, body) = function(&source, name);
    let query = extractor(params, "Query")
        .map(|query| fields(sources, query))
        .unwrap_or_default();

    let operation = |path: String, body| Operation {
        method: method.to_string(),
        path,
        query: query.clone(),
        body,
    };

    match extractor(params, "Json") {
        Some("Value") => body
            .match_indices("strip_suffix(\"")
            .map(|(index, _)| {
                let after = &body[index..];
                let suffix = string_literal(after);

                let call = &after
                    [after.find("return ").expect("suffix not dispatched") + "return ".len()..];
                let target = call[..call.find('(').unwrap()].trim();

                let (target_params, _) = function(&source, target);
                let typed = &target_params[target_params
                    .find("body: ")
                    .unwrap_or_else(|| panic!("fn {} takes no `body`", target))
                    + "body: ".len()..];
                let body_type = typed[..typed.find([',', ')']).unwrap_or(typed.len())].trim();

                operation(
                    format!("{}{}", path, suffix),
                    Some(fields(sources, body_type)),
                )
            })
            .collect(),
        Some(body_type) => vec![operation(
            path.to_string(),
            Some(fields(sources, body_type)),
        )],
        None => vec![operation(path.to_string(), None)],
    }
}

/// Every operation the content API serves, read from `build_router`. The
/// replication server is a separate router on its own listener, and the bare
/// `/` redirect sits outside `/v1`, so neither is part of the collection.
fn operations() -> Vec<Operation> {
    let main = strip_comments(&read(&root().join("src/main.rs")));
    let sources = sources();
    let (_, router) = function(&main, "build_router");

    // Routes registered on `health_routes` are nested under a prefix of their
    // own, read from the `nest("…", health_routes)` call.
    let health_from = router
        .find("let health_routes")
        .expect("no health router in build_router");
    let nest = &router[..router
        .find(", health_routes)")
        .expect("health router is never nested")];
    let health_prefix = &nest[nest[..nest.len() - 1].rfind('"').unwrap() + 1..nest.len() - 1];

    let mut operations = Vec::new();
    let mut cursor = 0;

    while let Some(found) = router[cursor..].find(".route(") {
        let open = cursor + found + ".route".len();
        let close = closing(router, open);
        let args = &router[open + 1..close];

        cursor = close;

        let path = string_literal(args);
        let chain = &args[args.find(',').unwrap() + 1..];

        if chain.contains("any(") {
            continue;
        }

        let prefix = if open > health_from {
            health_prefix
        } else {
            ""
        };

        for (method, handler) in handlers(chain) {
            operations.extend(expand(
                &sources,
                &method,
                &format!("{}{}", prefix, path),
                handler,
            ));
        }
    }

    operations
}

/// Every request file of the collection, sorted by path.
fn request_files() -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut directories = vec![root().join(COLLECTION)];

    while let Some(directory) = directories.pop() {
        for entry in fs::read_dir(&directory).unwrap() {
            let path = entry.unwrap().path();
            let name = path.file_name().unwrap().to_string_lossy().to_string();

            if path.is_dir() {
                if name != "environments" {
                    directories.push(path);
                }
            } else if name.ends_with(".yml") && name != "folder.yml" && name != "opencollection.yml"
            {
                files.push(path);
            }
        }
    }

    files.sort();

    files
}

/// A body's top-level keys: those of the JSON left once every `//` line is
/// dropped, and those of the `//` lines sitting at top-level indentation.
fn body_keys(file: &str, data: &str) -> (BTreeSet<String>, BTreeSet<String>) {
    let live = data
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");

    let live: Value = serde_json::from_str(&live).unwrap_or_else(|err| {
        panic!(
            "{}: body is not valid JSON once its comments are dropped: {}",
            file, err
        )
    });

    let live = live
        .as_object()
        .unwrap_or_else(|| panic!("{}: body is not a JSON object", file))
        .keys()
        .cloned()
        .collect();

    // The first key's indentation is the top level; a nested value's lines,
    // commented or not, sit deeper.
    let indent = data
        .lines()
        .nth(1)
        .map_or(0, |line| line.len() - line.trim_start().len());

    let commented = data
        .lines()
        .filter(|line| line.len() - line.trim_start().len() == indent)
        .filter_map(|line| {
            let key = line.trim_start().strip_prefix("// \"")?;

            Some(key[..key.find('"')?].to_string())
        })
        .collect();

    (live, commented)
}

fn request(path: &Path) -> Request {
    let file = path
        .strip_prefix(root().join(COLLECTION))
        .unwrap()
        .display()
        .to_string();

    let yaml = YamlLoader::load_from_str(&read(path))
        .unwrap_or_else(|err| panic!("{}: invalid YAML: {}", file, err))
        .remove(0);
    let http = &yaml["http"];

    let url = http["url"]
        .as_str()
        .unwrap_or_else(|| panic!("{}: no http.url", file));
    let url = url
        .strip_prefix(ENDPOINT)
        .unwrap_or_else(|| panic!("{}: url does not start with {}", file, ENDPOINT));
    let (path, query) = url.split_once('?').unwrap_or((url, ""));

    let params = http["params"]
        .as_vec()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter(|param| param["type"].as_str() == Some("query"))
        .map(|param| {
            (
                param["name"].as_str().unwrap().to_string(),
                param["disabled"].as_bool().unwrap_or(false),
            )
        })
        .collect();

    let body = match &http["body"] {
        Yaml::BadValue => None,
        body => Some(body_keys(
            &file,
            body["data"]
                .as_str()
                .unwrap_or_else(|| panic!("{}: body has no data", file)),
        )),
    };

    Request {
        method: http["method"].as_str().unwrap().to_uppercase(),
        path: path.to_string(),
        url_query: query
            .split('&')
            .filter(|pair| !pair.is_empty())
            .map(|pair| pair.split('=').next().unwrap().to_string())
            .collect(),
        params,
        body,
        file,
    }
}

fn segments(path: &str) -> Vec<&str> {
    path.split('/')
        .filter(|segment| !segment.is_empty())
        .collect()
}

/// Whether a request path (`{{variable}}` placeholders) fits a route pattern
/// (`{name}` takes one placeholder, `{*name}` one or more segments of any kind).
fn fits(request: &[&str], pattern: &[&str]) -> bool {
    match pattern.split_first() {
        None => request.is_empty(),
        Some((wildcard, rest)) if wildcard.starts_with("{*") => {
            (1..=request.len()).any(|taken| fits(&request[taken..], rest))
        }
        Some((parameter, rest)) if parameter.starts_with('{') => {
            request
                .first()
                .is_some_and(|segment| segment.starts_with("{{") && segment.ends_with("}}"))
                && fits(&request[1..], rest)
        }
        Some((literal, rest)) => request.first() == Some(literal) && fits(&request[1..], rest),
    }
}

/// What a request sends against what its route reads.
fn compare(request: &Request, operation: &Operation, problems: &mut Vec<String>) {
    let file = &request.file;

    // Query parameters: the same names on both sides, optional ones disabled
    // so the request runs with the defaults, required ones enabled.
    let sent: BTreeMap<&str, bool> = request
        .params
        .iter()
        .map(|(name, disabled)| (name.as_str(), *disabled))
        .collect();

    for (name, optional) in &operation.query {
        match sent.get(name.as_str()) {
            None => problems.push(format!("{}: query parameter `{}` is missing", file, name)),
            Some(false) if *optional => problems.push(format!(
                "{}: optional query parameter `{}` must be `disabled: true`",
                file, name
            )),
            Some(true) if !optional => problems.push(format!(
                "{}: required query parameter `{}` must not be disabled",
                file, name
            )),
            _ => {}
        }
    }

    for name in sent.keys() {
        if !operation.query.contains_key(*name) {
            problems.push(format!(
                "{}: query parameter `{}` is not read by {} {}",
                file, name, operation.method, operation.path
            ));
        }
    }

    // Bruno keeps the URL's query string in step with the enabled parameters.
    let enabled: Vec<String> = request
        .params
        .iter()
        .filter(|(_, disabled)| !disabled)
        .map(|(name, _)| name.clone())
        .collect();

    if enabled != request.url_query {
        problems.push(format!(
            "{}: url query string {:?} does not match the enabled parameters {:?}",
            file, request.url_query, enabled
        ));
    }

    // Body keys: required ones live, optional ones as `//` comment lines.
    match (&operation.body, &request.body) {
        (None, None) => {}
        (None, Some(_)) => problems.push(format!(
            "{}: sends a body that {} {} does not read",
            file, operation.method, operation.path
        )),
        (Some(_), None) => problems.push(format!(
            "{}: {} {} reads a JSON body, but the request sends none",
            file, operation.method, operation.path
        )),
        (Some(fields), Some((live, commented))) => {
            for (name, optional) in fields {
                let problem = match (*optional, live.contains(name), commented.contains(name)) {
                    (_, false, false) => Some("is missing"),
                    (true, true, _) => Some("is optional, so it must be a `//` comment line"),
                    (false, false, true) => Some("is required, so it must not be commented out"),
                    _ => None,
                };

                if let Some(problem) = problem {
                    problems.push(format!("{}: body key `{}` {}", file, name, problem));
                }
            }

            for name in live.union(commented) {
                if !fields.contains_key(name) {
                    problems.push(format!(
                        "{}: body key `{}` is not read by {} {}",
                        file, name, operation.method, operation.path
                    ));
                }
            }
        }
    }
}

#[test]
fn collection_mirrors_the_api() {
    let operations = operations();
    let requests: Vec<Request> = request_files().iter().map(|path| request(path)).collect();

    let mut problems = Vec::new();
    let mut covered = BTreeSet::new();

    for request in &requests {
        let matching: Vec<&Operation> = operations
            .iter()
            .filter(|operation| {
                operation.method == request.method
                    && fits(&segments(&request.path), &segments(&operation.path))
            })
            .collect();

        match matching.as_slice() {
            [] => problems.push(format!(
                "{}: {} {} matches no route the API serves",
                request.file, request.method, request.path
            )),
            [operation] => {
                covered.insert((operation.method.as_str(), operation.path.as_str()));

                compare(request, operation, &mut problems);
            }
            several => problems.push(format!(
                "{}: {} {} matches several routes: {}",
                request.file,
                request.method,
                request.path,
                several
                    .iter()
                    .map(|operation| operation.path.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    }

    for operation in &operations {
        let key = (operation.method.as_str(), operation.path.as_str());

        if !covered.contains(&key) && !NOT_IN_COLLECTION.contains(&key) {
            problems.push(format!(
                "{} {} has no request in the collection",
                operation.method, operation.path
            ));
        }
    }

    for (method, path) in NOT_IN_COLLECTION {
        let served = operations
            .iter()
            .any(|operation| operation.method == *method && operation.path == *path);

        if !served {
            problems.push(format!(
                "{} {} is listed in NOT_IN_COLLECTION, but the API no longer serves it",
                method, path
            ));
        } else if covered.contains(&(*method, *path)) {
            problems.push(format!(
                "{} {} is listed in NOT_IN_COLLECTION, but the collection has a request for it",
                method, path
            ));
        }
    }

    assert!(
        problems.is_empty(),
        "the OpenCollection in {} has drifted from the API ({} problems):\n  - {}",
        COLLECTION,
        problems.len(),
        problems.join("\n  - ")
    );
}
