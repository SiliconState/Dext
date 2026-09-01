use ra_ap_syntax::{
    Edition, SourceFile, TextRange,
    ast::{self, AstNode, HasModuleItem, HasName},
};
use std::fmt::Write as _;

pub(crate) const INPUT_MAX_BYTES: usize = 8 * 1024 * 1024;
pub(crate) const SELECTOR_MAX_BYTES: usize = 1024;
pub(crate) const LINE_MAX: usize = 1_000_000;
const SUGGESTION_LIMIT: usize = 5;
const FUZZY_MAX_BYTES: usize = 256;

type WindowResult = Result<Option<((usize, usize), bool)>, String>;

pub(crate) enum Selector<'a> {
    Symbol(&'a str),
    Line(usize),
}

#[derive(Clone)]
struct Owner {
    target: String,
    trait_name: Option<String>,
}

struct Candidate {
    name: String,
    kind: &'static str,
    range: TextRange,
    owner: Option<Owner>,
}

struct RustIndex {
    candidates: Vec<Candidate>,
    impls: Vec<(String, TextRange)>,
    containers: Vec<TextRange>,
    incomplete: bool,
}

pub(crate) fn read(
    content: &str,
    display_path: &str,
    rust: bool,
    selector: Selector<'_>,
    context: usize,
    cap: usize,
) -> Result<String, String> {
    if content.len() > INPUT_MAX_BYTES {
        return Err(format!(
            "source exceeds the {INPUT_MAX_BYTES}-byte read_symbol limit"
        ));
    }
    if context > 50 {
        return Err("context must be at most 50 lines".to_string());
    }
    if let Selector::Symbol(symbol) = &selector {
        if symbol.len() > SELECTOR_MAX_BYTES {
            return Err(format!(
                "symbol selector exceeds the {SELECTOR_MAX_BYTES}-byte limit"
            ));
        }
        if symbol.chars().any(char::is_control) {
            return Err("symbol selector contains control characters".to_string());
        }
    }
    let starts = line_starts(content)?;
    if starts.is_empty() {
        return Err(format!("{display_path} is empty"));
    }
    let rust_index = rust.then(|| RustIndex::new(content)).transpose()?;
    let ((start, end), incomplete) = match selector {
        Selector::Line(line) => {
            if line == 0 || line > starts.len() {
                return Err(format!(
                    "line {line} is outside {display_path} ({} lines)",
                    starts.len()
                ));
            }
            if let Some(index) = rust_index.as_ref() {
                index.line_window(line - 1, &starts, context)
            } else {
                let (start, end) = generic_line_window(content, &starts, line - 1);
                (with_context(start, end, starts.len(), context), false)
            }
        }
        Selector::Symbol(symbol) => {
            let window = if let Some(index) = rust_index.as_ref() {
                index.symbol_window(content, &starts, symbol, context)?
            } else {
                generic_symbol_window(content, &starts, symbol, context)
                    .map(|window| (window, false))
            };
            let Some((window, incomplete)) = window else {
                let suggestions = rust_index.as_ref().map_or_else(
                    || generic_suggestions(content, &starts, symbol),
                    |index| index.suggestions(content, &starts, symbol),
                );
                return Err(format!(
                    "symbol '{symbol}' not found in {display_path}\n{suggestions}Search first with rg -n '{symbol}' {display_path}, then retry read_symbol with an exact symbol or line."
                ));
            };
            (window, incomplete)
        }
    };
    let mut rendered = render_window(content, &starts, start, end, cap);
    if incomplete {
        rendered.push_str("\n[incomplete Rust item: terminator not found before EOF]\n");
    }
    Ok(rendered)
}

fn line_starts(content: &str) -> Result<Vec<usize>, String> {
    if content.is_empty() {
        return Ok(Vec::new());
    }
    let mut starts = Vec::with_capacity((content.len() / 48).max(1));
    starts.push(0);
    for (index, byte) in content.bytes().enumerate() {
        if byte == b'\n' && index + 1 < content.len() {
            starts.push(index + 1);
            if starts.len() > LINE_MAX {
                return Err(format!("source exceeds the {LINE_MAX} line limit"));
            }
        }
    }
    Ok(starts)
}

fn source_line<'a>(content: &'a str, starts: &[usize], index: usize) -> Option<&'a str> {
    let start = *starts.get(index)?;
    let end = starts.get(index + 1).copied().unwrap_or(content.len());
    let line = content[start..end]
        .strip_suffix('\n')
        .unwrap_or(&content[start..end]);
    Some(line.strip_suffix('\r').unwrap_or(line))
}

fn offset_line(starts: &[usize], offset: usize) -> usize {
    starts
        .partition_point(|start| *start <= offset)
        .saturating_sub(1)
}

fn range_lines(range: TextRange, starts: &[usize]) -> (usize, usize) {
    let start = u32::from(range.start()) as usize;
    let end = u32::from(range.end()) as usize;
    (
        offset_line(starts, start),
        offset_line(starts, end.saturating_sub(1).max(start)),
    )
}

fn with_context(start: usize, end: usize, lines: usize, context: usize) -> (usize, usize) {
    (
        start.saturating_sub(context),
        end.saturating_add(context).min(lines.saturating_sub(1)),
    )
}

impl RustIndex {
    fn new(content: &str) -> Result<Self, String> {
        let parse = SourceFile::parse(content, Edition::CURRENT);
        let errors = parse.errors();
        let incomplete = !errors.is_empty()
            && errors.iter().all(|error| {
                u32::from(error.range().end()) as usize >= content.len().saturating_sub(1)
                    && error.to_string().contains("expected R_CURLY")
            });
        if !errors.is_empty() && !incomplete {
            let error = &errors[0];
            return Err(format!(
                "cannot safely read Rust source: {} at byte {}",
                error,
                u32::from(error.range().start())
            ));
        }
        let file = parse.tree();
        let mut index = Self {
            candidates: Vec::new(),
            impls: Vec::new(),
            containers: Vec::new(),
            incomplete,
        };
        index.collect_items(file.items(), None);
        Ok(index)
    }

    fn collect_items(&mut self, items: impl Iterator<Item = ast::Item>, module: Option<&str>) {
        for item in items {
            match item {
                ast::Item::Const(node) => self.push_named(&node, "const", module, None),
                ast::Item::Enum(node) => self.push_named(&node, "enum", module, None),
                ast::Item::Fn(node) => self.push_named(&node, "fn", module, None),
                ast::Item::Module(node) => {
                    let name = node.name().map(|name| clean_name(name.text()));
                    self.push_named(&node, "mod", module, None);
                    if let (Some(name), Some(list)) = (name, node.item_list()) {
                        let path = qualify(module, &name);
                        self.collect_items(list.items(), Some(&path));
                    }
                }
                ast::Item::Static(node) => self.push_named(&node, "static", module, None),
                ast::Item::Struct(node) => self.push_named(&node, "struct", module, None),
                ast::Item::Union(node) => self.push_named(&node, "union", module, None),
                ast::Item::TypeAlias(node) => self.push_named(&node, "type", module, None),
                ast::Item::Trait(node) => {
                    let target = node
                        .name()
                        .map(|name| qualify(module, &clean_name(name.text())));
                    self.push_named(&node, "trait", module, None);
                    if let (Some(target), Some(list)) = (target, node.assoc_item_list()) {
                        let owner = Owner {
                            target,
                            trait_name: None,
                        };
                        for item in list.assoc_items() {
                            self.push_assoc(item, Some(owner.clone()));
                        }
                    }
                }
                ast::Item::Impl(node) => {
                    let target = node
                        .self_ty()
                        .map(|ty| normalize_type(&ty.syntax().text().to_string(), module));
                    let trait_name = node
                        .trait_()
                        .map(|ty| normalize_type(&ty.syntax().text().to_string(), module));
                    if let Some(target) = target {
                        let range = node.syntax().text_range();
                        self.containers.push(range);
                        self.impls.push((target.clone(), range));
                        if let Some(list) = node.assoc_item_list() {
                            let owner = Owner { target, trait_name };
                            for item in list.assoc_items() {
                                self.push_assoc(item, Some(owner.clone()));
                            }
                        }
                    }
                }
                ast::Item::ExternBlock(node) => {
                    self.containers.push(node.syntax().text_range());
                    if let Some(list) = node.extern_item_list() {
                        for item in list.extern_items() {
                            self.push_extern(item, module);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn push_named<N: AstNode + HasName>(
        &mut self,
        node: &N,
        kind: &'static str,
        module: Option<&str>,
        owner: Option<Owner>,
    ) {
        if let Some(name) = node.name() {
            self.candidates.push(Candidate {
                name: clean_name(name.text()),
                kind,
                range: node.syntax().text_range(),
                owner: owner.or_else(|| {
                    module.map(|target| Owner {
                        target: target.to_string(),
                        trait_name: None,
                    })
                }),
            });
        }
    }

    fn push_assoc(&mut self, item: ast::AssocItem, owner: Option<Owner>) {
        match item {
            ast::AssocItem::Const(node) => self.push_named(&node, "const", None, owner),
            ast::AssocItem::Fn(node) => self.push_named(&node, "fn", None, owner),
            ast::AssocItem::TypeAlias(node) => self.push_named(&node, "type", None, owner),
            ast::AssocItem::MacroCall(_) => {}
        }
    }

    fn push_extern(&mut self, item: ast::ExternItem, module: Option<&str>) {
        match item {
            ast::ExternItem::Fn(node) => self.push_named(&node, "fn", module, None),
            ast::ExternItem::Static(node) => self.push_named(&node, "static", module, None),
            ast::ExternItem::TypeAlias(node) => self.push_named(&node, "type", module, None),
            ast::ExternItem::MacroCall(_) => {}
        }
    }

    fn line_window(&self, line: usize, starts: &[usize], context: usize) -> ((usize, usize), bool) {
        let range = self
            .candidates
            .iter()
            .map(|candidate| candidate.range)
            .chain(self.containers.iter().copied())
            .filter(|range| {
                let (start, end) = range_lines(*range, starts);
                start <= line && line <= end
            })
            .min_by_key(|range| range.len());
        let (start, end) = range.map_or((line, line), |range| range_lines(range, starts));
        (
            with_context(start, end, starts.len(), context),
            self.incomplete && end + 1 == starts.len(),
        )
    }

    fn symbol_window(
        &self,
        content: &str,
        starts: &[usize],
        symbol: &str,
        context: usize,
    ) -> WindowResult {
        if let Some(target) = symbol
            .strip_prefix("impl ")
            .and_then(normalize_selector_path)
        {
            return self.unique_range(
                starts,
                symbol,
                self.impls
                    .iter()
                    .filter(|(candidate, _)| target_matches(candidate, &target))
                    .map(|(_, range)| *range),
                context,
            );
        }
        let selector = ParsedSelector::parse(symbol)?;
        let matches = self.candidates.iter().filter(|candidate| {
            candidate.name == selector.name
                && selector.owner.as_deref().is_none_or(|owner| {
                    candidate.owner.as_ref().is_some_and(|candidate_owner| {
                        target_matches(&candidate_owner.target, owner)
                            && selector.trait_name.as_deref().is_none_or(|trait_name| {
                                candidate_owner.trait_name.as_deref().is_some_and(
                                    |candidate_trait| target_matches(candidate_trait, trait_name),
                                )
                            })
                    })
                })
        });
        let mut matches = matches.peekable();
        let Some(first) = matches.next() else {
            return Ok(None);
        };
        if matches.peek().is_some() {
            let mut message = format!("symbol '{symbol}' is ambiguous; candidates:\n");
            for candidate in std::iter::once(first).chain(matches).take(SUGGESTION_LIMIT) {
                let line = range_lines(candidate.range, starts).0;
                let _ = writeln!(
                    message,
                    "- {} at line {} — {}",
                    candidate.qualified_name(),
                    line + 1,
                    source_line(content, starts, line).unwrap_or("").trim()
                );
            }
            return Err(message);
        }
        let lines = range_lines(first.range, starts);
        Ok(Some((
            with_context(lines.0, lines.1, starts.len(), context),
            self.incomplete && lines.1 + 1 == starts.len(),
        )))
    }

    fn unique_range(
        &self,
        starts: &[usize],
        symbol: &str,
        matches: impl Iterator<Item = TextRange>,
        context: usize,
    ) -> WindowResult {
        let mut matches = matches.peekable();
        let Some(range) = matches.next() else {
            return Ok(None);
        };
        if matches.peek().is_some() {
            return Err(format!(
                "symbol '{symbol}' is ambiguous; qualify the impl or use a line selector"
            ));
        }
        let lines = range_lines(range, starts);
        Ok(Some((
            with_context(lines.0, lines.1, starts.len(), context),
            self.incomplete && lines.1 + 1 == starts.len(),
        )))
    }

    fn suggestions(&self, content: &str, starts: &[usize], query: &str) -> String {
        ranked_suggestions(
            self.candidates.iter().map(|candidate| {
                let line = range_lines(candidate.range, starts).0;
                (
                    candidate.name.as_str(),
                    candidate.kind,
                    line,
                    source_line(content, starts, line).unwrap_or(""),
                )
            }),
            query,
        )
    }
}

impl Candidate {
    fn qualified_name(&self) -> String {
        let Some(owner) = &self.owner else {
            return self.name.clone();
        };
        if let Some(trait_name) = &owner.trait_name {
            format!("<{} as {trait_name}>::{}", owner.target, self.name)
        } else {
            format!("{}::{}", owner.target, self.name)
        }
    }
}

struct ParsedSelector {
    owner: Option<String>,
    trait_name: Option<String>,
    name: String,
}

impl ParsedSelector {
    fn parse(symbol: &str) -> Result<Self, String> {
        if let Some(rest) = symbol.strip_prefix('<')
            && let Some((qualification, name)) = rest.rsplit_once(">::")
            && let Some((target, trait_name)) = qualification.split_once(" as ")
        {
            return Ok(Self {
                owner: normalize_selector_path(target),
                trait_name: normalize_selector_path(trait_name),
                name: clean_name(name.trim()),
            });
        }
        if let Some((owner, name)) = split_selector(symbol) {
            return Ok(Self {
                owner: normalize_selector_path(owner),
                trait_name: None,
                name: clean_name(name.trim()),
            });
        }
        if symbol.is_empty() {
            return Err("Rust symbol selector must not be empty".to_string());
        }
        Ok(Self {
            owner: None,
            trait_name: None,
            name: clean_name(symbol),
        })
    }
}

fn split_selector(symbol: &str) -> Option<(&str, &str)> {
    let path = symbol.rfind("::").map(|index| (index, 2));
    let dot = symbol.rfind('.').map(|index| (index, 1));
    let (index, width) = match (path, dot) {
        (Some(path), Some(dot)) => path.max(dot),
        (path, dot) => path.or(dot)?,
    };
    Some((&symbol[..index], &symbol[index + width..]))
}

fn normalize_selector_path(path: &str) -> Option<String> {
    let compact = path
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect::<String>()
        .replace("r#", "");
    (!compact.is_empty()).then_some(compact)
}

fn normalize_type(text: &str, module: Option<&str>) -> String {
    let compact = normalize_selector_path(text).unwrap_or_default();
    if let Some(rest) = compact.strip_prefix("self::") {
        return qualify(module, rest);
    }
    if compact.starts_with("super::") {
        let mut module = module.unwrap_or("");
        let mut rest = compact.as_str();
        while let Some(next) = rest.strip_prefix("super::") {
            rest = next;
            module = module.rsplit_once("::").map_or("", |(parent, _)| parent);
        }
        return qualify((!module.is_empty()).then_some(module), rest);
    }
    if compact.contains("::")
        || compact.starts_with('<')
        || compact.starts_with('&')
        || compact
            .chars()
            .next()
            .is_some_and(|character| !character.is_alphabetic() && character != '_')
    {
        compact
    } else {
        qualify(module, &compact)
    }
}

fn qualify(module: Option<&str>, name: &str) -> String {
    module.map_or_else(|| name.to_string(), |module| format!("{module}::{name}"))
}

fn clean_name(name: &str) -> String {
    name.strip_prefix("r#").unwrap_or(name).to_string()
}

fn canonical_target(target: &str) -> &str {
    target
        .strip_prefix("crate::")
        .or_else(|| target.strip_prefix("self::"))
        .or_else(|| target.strip_prefix("::"))
        .unwrap_or(target)
}

fn target_matches(candidate: &str, selector: &str) -> bool {
    let candidate = canonical_target(candidate);
    let selector = canonical_target(selector);
    let exact = candidate == selector
        || (!selector.contains("::")
            && candidate
                .strip_suffix(selector)
                .is_some_and(|prefix| prefix.ends_with("::")));
    exact
        || (!selector.contains('<')
            && candidate
                .find('<')
                .is_some_and(|generic| target_matches(&candidate[..generic], selector)))
}

fn generic_line_window(content: &str, starts: &[usize], target: usize) -> (usize, usize) {
    generic_enclosing_block(content, starts, target)
        .unwrap_or_else(|| paragraph_window(content, starts, target))
}

fn generic_enclosing_block(
    content: &str,
    starts: &[usize],
    target: usize,
) -> Option<(usize, usize)> {
    let mut stack = Vec::new();
    let mut fallback = None;
    for line_index in 0..starts.len() {
        let line = source_line(content, starts, line_index)?;
        for byte in line.bytes() {
            match byte {
                b'{' => {
                    let item = generic_item_start(content, starts, line_index);
                    stack.push((item.unwrap_or(line_index), item.is_some()));
                }
                b'}' => {
                    if let Some((start, item)) = stack.pop()
                        && start <= target
                        && target <= line_index
                    {
                        if item {
                            return Some((start, line_index));
                        }
                        fallback.get_or_insert((start, line_index));
                    }
                }
                _ => {}
            }
        }
    }
    fallback
}

fn generic_item_start(content: &str, starts: &[usize], open_line: usize) -> Option<usize> {
    let minimum = open_line.saturating_sub(12);
    for index in (minimum..=open_line).rev() {
        let line = source_line(content, starts, index)?.trim();
        if generic_item_signature(line) {
            return Some(index);
        }
        if index < open_line && line.is_empty() {
            break;
        }
    }
    None
}

fn generic_item_signature(line: &str) -> bool {
    let line = strip_qualifiers(line);
    [
        "fn",
        "struct",
        "enum",
        "trait",
        "impl",
        "type",
        "mod",
        "class",
        "def",
        "func",
        "function",
        "interface",
    ]
    .into_iter()
    .any(|item| keyword(line, item).is_some())
}

fn generic_symbol_window(
    content: &str,
    starts: &[usize],
    symbol: &str,
    context: usize,
) -> Option<(usize, usize)> {
    let index = (0..starts.len()).find(|index| {
        source_line(content, starts, *index).is_some_and(|line| declaration(line, symbol))
    })?;
    let mut end = index;
    let mut braces = 0i32;
    let mut opened = false;
    for line_index in index..starts.len() {
        let line = source_line(content, starts, line_index)?;
        for byte in line.bytes() {
            match byte {
                b'{' => {
                    braces += 1;
                    opened = true;
                }
                b'}' => braces -= 1,
                _ => {}
            }
        }
        end = line_index;
        if opened && braces <= 0 {
            break;
        }
        if !opened && line_index > index && line.trim().is_empty() {
            end = line_index - 1;
            break;
        }
    }
    Some(with_context(index, end, starts.len(), context))
}

fn paragraph_window(content: &str, starts: &[usize], target: usize) -> (usize, usize) {
    let mut start = target;
    while start > 0
        && source_line(content, starts, start - 1).is_some_and(|line| !line.trim().is_empty())
    {
        start -= 1;
    }
    let mut end = target;
    while end + 1 < starts.len()
        && source_line(content, starts, end + 1).is_some_and(|line| !line.trim().is_empty())
    {
        end += 1;
    }
    (start, end)
}

fn declaration(line: &str, symbol: &str) -> bool {
    let mut text = strip_qualifiers(line);
    if let Some(rest) = keyword(text, "const").and_then(|rest| keyword(rest, "fn")) {
        text = rest;
    } else {
        for candidate in [
            "fn",
            "struct",
            "enum",
            "trait",
            "type",
            "mod",
            "class",
            "def",
            "func",
            "function",
            "interface",
            "const",
            "static",
        ] {
            if let Some(rest) = keyword(text, candidate) {
                text = rest;
                break;
            }
        }
    }
    identifier(text).is_some_and(|name| clean_name(name) == symbol)
}

fn strip_qualifiers(mut text: &str) -> &str {
    loop {
        let trimmed = text.trim_start();
        if let Some(rest) = [
            "pub ",
            "public ",
            "private ",
            "protected ",
            "export ",
            "default ",
            "async ",
            "auto ",
            "unsafe ",
        ]
        .into_iter()
        .find_map(|prefix| trimmed.strip_prefix(prefix))
        {
            text = rest;
        } else if let Some(rest) = trimmed.strip_prefix("pub(") {
            let Some(end) = rest.find(')') else {
                return trimmed;
            };
            text = &rest[end + 1..];
        } else if let Some(rest) = trimmed.strip_prefix("extern ") {
            text = rest
                .trim_start()
                .strip_prefix('"')
                .and_then(|abi| abi.find('"').map(|end| &abi[end + 1..]))
                .unwrap_or(rest);
        } else {
            return trimmed;
        }
    }
}

fn keyword<'a>(text: &'a str, keyword: &str) -> Option<&'a str> {
    let rest = text.trim_start().strip_prefix(keyword)?;
    rest.chars()
        .next()
        .is_none_or(|character| !is_ident_continue(character))
        .then(|| rest.trim_start())
}

fn identifier(text: &str) -> Option<&str> {
    let text = text.trim_start();
    let raw = text.strip_prefix("r#").unwrap_or(text);
    let mut end = 0;
    for (index, character) in raw.char_indices() {
        if is_ident_continue(character) {
            end = index + character.len_utf8();
        } else {
            break;
        }
    }
    (end > 0).then(|| {
        if raw.len() == text.len() {
            &text[..end]
        } else {
            &text[..end + 2]
        }
    })
}

fn is_ident_continue(character: char) -> bool {
    character == '_' || character.is_alphanumeric()
}

fn generic_suggestions(content: &str, starts: &[usize], query: &str) -> String {
    ranked_suggestions(
        (0..starts.len()).filter_map(|line| {
            let source = source_line(content, starts, line)?;
            let name = [
                "fn",
                "struct",
                "enum",
                "trait",
                "type",
                "mod",
                "class",
                "def",
                "func",
                "function",
                "interface",
                "const",
                "static",
            ]
            .into_iter()
            .find_map(|kind| {
                keyword(strip_qualifiers(source), kind)
                    .and_then(identifier)
                    .map(|name| (name, kind))
            })?;
            Some((name.0, name.1, line, source))
        }),
        query,
    )
}

fn ranked_suggestions<'a>(
    candidates: impl Iterator<Item = (&'a str, &'static str, usize, &'a str)>,
    query: &str,
) -> String {
    if query.len() > FUZZY_MAX_BYTES {
        return String::new();
    }
    let mut suggestions = candidates
        .filter_map(|(name, kind, line, preview)| {
            fuzzy_score(query, name).map(|score| (score, name, kind, line, preview))
        })
        .collect::<Vec<_>>();
    suggestions.sort_by_key(|(score, _, _, line, _)| (*score, *line));
    if suggestions.is_empty() {
        return String::new();
    }
    let mut output = String::from("Did you mean:\n");
    for (_, name, kind, line, preview) in suggestions.into_iter().take(SUGGESTION_LIMIT) {
        let _ = writeln!(
            output,
            "- {name} at line {} ({kind}) — {}",
            line + 1,
            preview.trim()
        );
    }
    output
}

fn fuzzy_score(query: &str, candidate: &str) -> Option<usize> {
    let query = query.to_ascii_lowercase();
    let candidate = candidate.to_ascii_lowercase();
    if query == candidate {
        return Some(0);
    }
    if candidate.starts_with(&query) {
        return Some(10 + candidate.len().saturating_sub(query.len()));
    }
    if candidate.contains(&query) {
        return Some(30 + candidate.len().saturating_sub(query.len()));
    }
    let distance = levenshtein(&query, &candidate);
    (distance <= (query.len().max(candidate.len()) / 3).max(2)).then_some(50 + distance * 4)
}

fn levenshtein(left: &str, right: &str) -> usize {
    let mut previous = (0..=right.chars().count()).collect::<Vec<_>>();
    let mut current = vec![0; previous.len()];
    for (row, left) in left.chars().enumerate() {
        current[0] = row + 1;
        for (column, right) in right.chars().enumerate() {
            current[column + 1] = (previous[column + 1] + 1)
                .min(current[column] + 1)
                .min(previous[column] + usize::from(left != right));
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[right.chars().count()]
}

fn render_window(content: &str, starts: &[usize], start: usize, end: usize, cap: usize) -> String {
    let mut output = String::new();
    for index in start..=end {
        let Some(line) = source_line(content, starts, index) else {
            continue;
        };
        let rendered = format!("{}\t{}\n", index + 1, line);
        if output.len().saturating_add(rendered.len()) > cap {
            let _ = write!(
                output,
                "\n…[output capped; use read_file offset={} to continue]\n",
                index + 1
            );
            break;
        }
        output.push_str(&rendered);
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rust(content: &str, selector: Selector<'_>) -> Result<String, String> {
        read(content, "lib.rs", true, selector, 0, 16_000)
    }

    #[test]
    fn rust_parser_ignores_literals_and_rejects_ambiguous_names() {
        let source = r##"
impl First { fn new() { let _ = r#"}"#; } }
impl Second { fn new() { /* } */ } }
"##;
        let error = rust(source, Selector::Symbol("new")).unwrap_err();
        assert!(error.contains("ambiguous"), "{error}");
        let second = rust(source, Selector::Symbol("Second::new")).unwrap();
        assert!(second.contains("impl Second"), "{second}");
    }

    #[test]
    fn rust_parser_includes_outer_attributes_and_docs() {
        let source = "//! inner\n/// docs\n#[cfg(any())]\nfn target() {}\n";
        let output = rust(source, Selector::Symbol("target")).unwrap();
        assert!(output.starts_with("2\t/// docs"), "{output}");
        assert!(!output.contains("inner"), "{output}");
    }

    #[test]
    fn rust_parser_excludes_local_and_macro_functions() {
        let source =
            "fn outer() { fn local() {} }\nmacro_rules! m { () => { fn generated() {} } }\n";
        assert!(
            rust(source, Selector::Symbol("local"))
                .unwrap_err()
                .contains("not found")
        );
        assert!(
            rust(source, Selector::Symbol("generated"))
                .unwrap_err()
                .contains("not found")
        );
    }

    #[test]
    fn rust_parser_resolves_trait_ufcs() {
        let source = "trait Build { fn new() -> Self; }\nstruct Item;\nimpl Build for Item { fn new() -> Self { Item } }\n";
        let output = rust(source, Selector::Symbol("<Item as Build>::new")).unwrap();
        assert!(output.contains("impl Build for Item"), "{output}");
    }

    #[test]
    fn rust_parser_handles_generic_relative_and_impl_selectors() {
        let source =
            "mod outer { struct Thing; mod inner { impl<T> super::Thing { fn build() {} } } }\n";
        let method = rust(source, Selector::Symbol("outer::Thing::build")).unwrap();
        assert!(method.contains("fn build"), "{method}");
        let implementation = rust(source, Selector::Symbol("impl outer::Thing")).unwrap();
        assert!(
            implementation.contains("impl<T> super::Thing"),
            "{implementation}"
        );
    }

    #[test]
    fn line_mode_uses_ast_item_and_attachment_ranges() {
        let source = "/// docs\n#[allow(dead_code)]\nfn target() {\n    let value = 1;\n}\n";
        let output = rust(source, Selector::Line(4)).unwrap();
        assert!(output.starts_with("1\t/// docs"), "{output}");
        assert!(output.contains("5\t}"), "{output}");
    }

    #[test]
    fn module_api_enforces_input_selector_and_context_bounds() {
        let oversized = "x".repeat(INPUT_MAX_BYTES + 1);
        assert!(
            read(&oversized, "large.rs", true, Selector::Line(1), 0, 1024,)
                .unwrap_err()
                .contains("byte read_symbol limit")
        );
        assert!(
            read(
                "fn ok() {}\n",
                "lib.rs",
                true,
                Selector::Symbol(&"x".repeat(SELECTOR_MAX_BYTES + 1)),
                0,
                1024,
            )
            .unwrap_err()
            .contains("selector exceeds")
        );
        assert!(
            read(
                "fn ok() {}\n",
                "lib.rs",
                true,
                Selector::Symbol("ok\nother"),
                0,
                1024,
            )
            .unwrap_err()
            .contains("control characters")
        );
        assert!(
            read("fn ok() {}\n", "lib.rs", true, Selector::Line(1), 51, 1024,)
                .unwrap_err()
                .contains("at most 50")
        );
    }

    #[test]
    fn parser_indexes_its_own_source() {
        let source = include_str!("read_symbol.rs");
        let output = rust(source, Selector::Symbol("RustIndex::new")).unwrap();
        assert!(output.contains("fn new"), "{output}");
    }

    #[test]
    fn malformed_rust_fails_closed_but_incomplete_item_is_marked() {
        let malformed =
            rust("fn broken() { let x = \"; }", Selector::Symbol("broken")).unwrap_err();
        assert!(
            malformed.contains("cannot safely read Rust source"),
            "{malformed}"
        );
        let incomplete = rust("fn pending() {\n", Selector::Symbol("pending")).unwrap();
        assert!(incomplete.contains("incomplete Rust item"), "{incomplete}");
    }
}
