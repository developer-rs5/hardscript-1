use hs_compiler::token::Kw;
use hs_compiler::{Span, Tok, Token};
use tower_lsp::lsp_types::{Position, Range};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WordRange {
    pub start: usize,
    pub end: usize,
    pub text: String,
}

#[derive(Clone)]
pub struct TextIndex {
    source: String,
    line_starts: Vec<usize>,
    checkpoint_starts: Vec<u32>,
    checkpoints: Vec<(u32, u32)>,
}

const CHECKPOINT_STRIDE: usize = 32;

impl TextIndex {
    pub fn new(source: String) -> TextIndex {
        let mut line_starts = vec![0];
        let mut checkpoint_starts = vec![0u32];
        let mut checkpoints: Vec<(u32, u32)> = Vec::new();
        let mut line_start = 0usize;
        let mut column = 0u32;
        let mut characters = 0usize;
        for (offset, character) in source.char_indices() {
            if character == '\n' {
                line_starts.push(offset + 1);
                line_start = offset + 1;
                checkpoint_starts.push(checkpoints.len() as u32);
                column = 0;
                characters = 0;
                continue;
            }
            if characters % CHECKPOINT_STRIDE == 0 {
                checkpoints.push(((offset - line_start) as u32, column));
            }
            column += character.len_utf16() as u32;
            characters += 1;
        }
        checkpoint_starts.push(checkpoints.len() as u32);
        TextIndex {
            source,
            line_starts,
            checkpoint_starts,
            checkpoints,
        }
    }

    fn line_checkpoints(&self, line: usize) -> &[(u32, u32)] {
        let start = self.checkpoint_starts[line] as usize;
        let end = self.checkpoint_starts[line + 1] as usize;
        &self.checkpoints[start..end]
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    pub fn line_count(&self) -> usize {
        self.line_starts.len()
    }

    pub fn full_range(&self) -> Range {
        Range::new(Position::new(0, 0), self.end_position())
    }

    pub fn end_position(&self) -> Position {
        self.position(self.source.len())
    }

    pub fn position(&self, offset: usize) -> Position {
        let mut offset = offset.min(self.source.len());
        while offset > 0 && !self.source.is_char_boundary(offset) {
            offset -= 1;
        }
        let line = match self.line_starts.binary_search(&offset) {
            Ok(index) => index,
            Err(index) => index.saturating_sub(1),
        };
        let start = self.line_starts[line];
        let relative = (offset - start) as u32;
        let checkpoints = self.line_checkpoints(line);
        let index = checkpoints.partition_point(|(byte, _)| *byte <= relative);
        let mut character = match index.checked_sub(1) {
            Some(position) => checkpoints[position].1,
            None => 0,
        };
        let from = start
            + checkpoints
                .get(index.wrapping_sub(1))
                .map_or(0, |(byte, _)| *byte) as usize;
        for value in self.source[from..offset].chars() {
            character += value.len_utf16() as u32;
        }
        Position::new(line as u32, character)
    }

    pub fn offset(&self, position: Position) -> usize {
        let line = position.line as usize;
        if line >= self.line_starts.len() {
            return self.source.len();
        }
        let start = self.line_starts[line];
        let end = self
            .line_starts
            .get(line + 1)
            .copied()
            .unwrap_or(self.source.len());
        let wanted = position.character;
        let checkpoints = self.line_checkpoints(line);
        let index = checkpoints.partition_point(|(_, column)| *column <= wanted);
        let (byte, mut column) = match index.checked_sub(1) {
            Some(position) => checkpoints[position],
            None => (0, 0),
        };
        let mut offset = start + byte as usize;
        for character in self.source[offset..end].chars() {
            if column >= wanted {
                break;
            }
            let width = character.len_utf16() as u32;
            if column + width > wanted {
                break;
            }
            column += width;
            offset += character.len_utf8();
        }
        offset.min(end)
    }

    pub fn span_start(&self, span: Span) -> Position {
        Position::new(
            span.line.saturating_sub(1) as u32,
            span.col.saturating_sub(1) as u32,
        )
    }

    pub fn span_range(&self, span: Span, end: Position) -> Range {
        let start = self.span_start(span);
        let end = if end < start {
            self.advance(start, 1)
        } else {
            end
        };
        Range::new(start, end)
    }

    pub fn advance(&self, position: Position, count: usize) -> Position {
        let offset = self.offset(position);
        let mut remaining = count;
        let mut cursor = offset;
        while remaining > 0 && cursor < self.source.len() {
            let character = self.source[cursor..].chars().next().unwrap_or('\n');
            cursor += character.len_utf8();
            remaining -= 1;
        }
        self.position(cursor)
    }

    pub fn text_range(&self, start: usize, end: usize) -> Range {
        let start = start.min(self.source.len());
        let end = end.clamp(start, self.source.len());
        Range::new(self.position(start), self.position(end))
    }

    pub fn line_range(&self, line: u32) -> Range {
        let line = line as usize;
        if line >= self.line_starts.len() {
            return self.full_range();
        }
        let start = self.line_starts[line];
        let end = self
            .line_starts
            .get(line + 1)
            .copied()
            .unwrap_or(self.source.len());
        self.text_range(start, end)
    }

    pub fn line_text(&self, line: u32) -> &str {
        let line = line as usize;
        if line >= self.line_starts.len() {
            return "";
        }
        let start = self.line_starts[line];
        let end = self
            .line_starts
            .get(line + 1)
            .copied()
            .unwrap_or(self.source.len());
        self.source[start..end].trim_end_matches(['\r', '\n'])
    }

    pub fn token_offset(&self, span: Span) -> usize {
        self.offset(self.span_start(span))
    }

    pub fn token_range(&self, tokens: &[Token], index: usize) -> Range {
        if index >= tokens.len() {
            return self.full_range();
        }
        let start = self.token_offset(tokens[index].span);
        let end = self.token_end_offset(&tokens[index]);
        self.text_range(start, end.max(start))
    }

    pub fn token_end(&self, tokens: &[Token], index: usize) -> Position {
        self.token_range(tokens, index).end
    }

    pub fn token_text(&self, tokens: &[Token], index: usize) -> &str {
        if index >= tokens.len() {
            return "";
        }
        let range = self.token_range(tokens, index);
        self.source
            .get(self.offset(range.start)..self.offset(range.end))
            .unwrap_or("")
    }

    fn token_end_offset(&self, token: &Token) -> usize {
        let start = self.token_offset(token.span).min(self.source.len());
        if matches!(token.tok, Tok::Eof) {
            return start;
        }
        let rest = self.source.get(start..).unwrap_or("");
        let width = match &token.tok {
            Tok::Ident(_) => rest
                .chars()
                .take_while(|character| is_identifier_continue(*character))
                .map(char::len_utf8)
                .sum::<usize>(),
            Tok::Kw(keyword) => keyword_text(*keyword)
                .and_then(|value| rest.starts_with(value).then_some(value.len()))
                .unwrap_or(0),
            Tok::Int(_) | Tok::Float(_) => rest
                .chars()
                .take_while(|character| character.is_ascii_digit() || *character == '.')
                .map(char::len_utf8)
                .sum::<usize>(),
            Tok::Str(_) => string_token_width(rest),
            Tok::Sym(symbol) => rest
                .starts_with(symbol.as_str())
                .then_some(symbol.as_str().len())
                .unwrap_or_else(|| rest.chars().next().map(char::len_utf8).unwrap_or(0)),
            Tok::Eof => 0,
        };
        (start + width).min(self.source.len())
    }

    pub fn word_at(&self, position: Position) -> Option<WordRange> {
        let offset = self.offset(position);
        self.word_at_offset(offset)
    }

    pub fn word_before(&self, position: Position) -> Option<WordRange> {
        let mut offset = self.offset(position);
        while offset > 0 {
            let previous = self.source[..offset].chars().next_back().unwrap_or(' ');
            if is_identifier_continue(previous) {
                offset -= previous.len_utf8();
            } else {
                break;
            }
        }
        self.word_at_offset(offset)
    }

    pub fn word_at_offset(&self, offset: usize) -> Option<WordRange> {
        if offset > self.source.len() || self.inside_string(offset) {
            return None;
        }
        let mut start = offset;
        while start > 0 {
            let previous = self.source[..start].chars().next_back().unwrap_or(' ');
            if is_identifier_continue(previous) {
                start -= previous.len_utf8();
            } else {
                break;
            }
        }
        let mut end = offset;
        while end < self.source.len() {
            let next = self.source[end..].chars().next().unwrap_or(' ');
            if is_identifier_continue(next) {
                end += next.len_utf8();
            } else {
                break;
            }
        }
        let text = self.source.get(start..end)?;
        if text.is_empty() || !text.chars().next().is_some_and(is_identifier_start) {
            return None;
        }
        Some(WordRange {
            start,
            end,
            text: text.to_string(),
        })
    }

    pub fn string_at(&self, position: Position) -> Option<(usize, usize, String)> {
        let offset = self.offset(position);
        let line_start = self.source[..offset]
            .rfind('\n')
            .map(|index| index + 1)
            .unwrap_or(0);
        let cursor = self.source.get(line_start..offset)?;
        let bytes = cursor.as_bytes();
        let mut quote = None;
        let mut escaped = false;
        for (index, byte) in bytes.iter().enumerate().rev() {
            if escaped {
                escaped = false;
                continue;
            }
            if *byte == b'\\' {
                escaped = true;
                continue;
            }
            if *byte == b'"' {
                quote = Some(index);
                break;
            }
        }
        let start = quote?;
        let absolute_start = line_start + start;
        let rest = self.source.get(absolute_start..)?;
        let mut escaped = false;
        let mut end = None;
        for (index, character) in rest.char_indices().skip(1) {
            if escaped {
                escaped = false;
                continue;
            }
            if character == '\\' {
                escaped = true;
            } else if character == '"' {
                end = Some(absolute_start + index + character.len_utf8());
                break;
            }
        }
        let content_end = end.unwrap_or(self.source.len());
        let raw = self.source.get(absolute_start..content_end)?;
        let content = raw
            .strip_prefix('"')
            .unwrap_or(raw)
            .strip_suffix('"')
            .unwrap_or_else(|| raw.strip_prefix('"').unwrap_or(raw));
        Some((absolute_start, content_end, content.to_string()))
    }

    fn inside_string(&self, offset: usize) -> bool {
        let mut in_string = false;
        let mut escaped = false;
        for (index, character) in self.source.char_indices() {
            if index >= offset {
                break;
            }
            if in_string {
                if escaped {
                    escaped = false;
                } else if character == '\\' {
                    escaped = true;
                } else if character == '"' {
                    in_string = false;
                }
            } else if character == '"' {
                in_string = true;
            }
        }
        in_string
    }

    pub fn is_import_string(&self, position: Position) -> bool {
        let offset = self.offset(position);
        let line_start = self.source[..offset]
            .rfind('\n')
            .map(|value| value + 1)
            .unwrap_or(0);
        let before = self.source.get(line_start..offset).unwrap_or("");
        before.trim_start().starts_with("bring ")
    }
}

fn keyword_text(keyword: Kw) -> Option<&'static str> {
    Some(match keyword {
        Kw::Bring => "bring",
        Kw::App => "app",
        Kw::Model => "model",
        Kw::Run => "run",
        Kw::Get => "GET",
        Kw::Post => "POST",
        Kw::Put => "PUT",
        Kw::Delete => "DELETE",
        Kw::Patch => "PATCH",
        Kw::Socket => "socket",
        Kw::Connect => "connect",
        Kw::Message => "message",
        Kw::Disconnect => "disconnect",
        Kw::Before => "before",
        Kw::Loop => "loop",
        Kw::Pick => "pick",
        Kw::Async => "async",
        Kw::Wait => "wait",
        Kw::Race => "race",
        Kw::Test => "test",
        Kw::Expect => "expect",
        Kw::Calc => "calc",
    })
}

fn string_token_width(value: &str) -> usize {
    if value.starts_with("\"\"\"") {
        let mut cursor = 3usize;
        while cursor < value.len() {
            if value[cursor..].starts_with("\"\"\"") {
                return cursor + 3;
            }
            cursor += value[cursor..]
                .chars()
                .next()
                .map(char::len_utf8)
                .unwrap_or(1);
        }
        return value.len();
    }
    let mut cursor = 1usize;
    let mut escaped = false;
    while cursor < value.len() {
        let character = value[cursor..].chars().next().unwrap_or('"');
        if escaped {
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else if character == '"' {
            return cursor + character.len_utf8();
        }
        cursor += character.len_utf8();
    }
    value.len()
}

pub fn is_identifier_start(character: char) -> bool {
    character == '_' || character.is_alphabetic()
}

pub fn is_identifier_continue(character: char) -> bool {
    character == '_' || character.is_alphanumeric()
}

pub fn is_identifier(value: &str) -> bool {
    let mut characters = value.chars();
    characters.next().is_some_and(is_identifier_start) && characters.all(is_identifier_continue)
}

pub fn contains_position(position: Position, range: Range) -> bool {
    position >= range.start && position <= range.end
}

pub fn fuzzy_score(candidate: &str, query: &str) -> Option<i32> {
    if query.is_empty() {
        return Some(10_000);
    }
    let candidate_lower = candidate.to_lowercase();
    let query_lower = query.to_lowercase();
    if candidate_lower == query_lower {
        return Some(20_000);
    }
    if candidate_lower.starts_with(&query_lower) {
        return Some(16_000 - candidate.len() as i32);
    }
    let mut score = 8_000i32;
    let mut cursor = 0usize;
    let mut previous = None;
    for character in query_lower.chars() {
        let Some(found) = candidate_lower[cursor..]
            .chars()
            .position(|item| item == character)
        else {
            return None;
        };
        let index = cursor + found;
        if previous == Some(index) {
            score += 4;
        }
        if index == 0 || candidate_lower[..index].ends_with(['.', '_', '-', '/']) {
            score += 8;
        }
        previous = Some(index);
        cursor = index
            + candidate_lower[index..]
                .chars()
                .next()
                .map(char::len_utf8)
                .unwrap_or(1);
    }
    Some(score - candidate.len() as i32)
}

pub fn token_is_keyword(token: &Token) -> bool {
    matches!(token.tok, Tok::Kw(_))
}

pub fn common_change_range(original: &str, changed: &str) -> Option<(usize, usize, usize)> {
    if original == changed {
        return None;
    }
    let mut prefix = original
        .bytes()
        .zip(changed.bytes())
        .take_while(|(left, right)| left == right)
        .count();
    while prefix > 0 && (!original.is_char_boundary(prefix) || !changed.is_char_boundary(prefix)) {
        prefix -= 1;
    }
    let mut suffix = 0usize;
    while original.len() - suffix > prefix
        && changed.len() - suffix > prefix
        && original.as_bytes()[original.len() - suffix - 1]
            == changed.as_bytes()[changed.len() - suffix - 1]
    {
        suffix += 1;
    }
    while suffix > 0
        && (!original.is_char_boundary(original.len() - suffix)
            || !changed.is_char_boundary(changed.len() - suffix))
    {
        suffix -= 1;
    }
    Some((prefix, original.len() - suffix, changed.len() - suffix))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_positions_round_trip() {
        let index = TextIndex::new("a😀b\nsecond\r\n".to_string());
        let position = index.position("a😀".len());
        assert_eq!(position, Position::new(0, 3));
        assert_eq!(index.offset(position), "a😀".len());
        assert_eq!(index.position("a😀b\nsecond".len()), Position::new(1, 6));
    }

    #[test]
    fn words_exclude_strings() {
        let index = TextIndex::new("value <- \"value\"\n".to_string());
        let word = index.word_at(Position::new(0, 2)).unwrap();
        assert_eq!(word.text, "value");
        assert!(index.word_at(Position::new(0, 13)).is_none());
    }

    #[test]
    fn fuzzy_ranking_prefers_prefix_and_case() {
        assert!(fuzzy_score("calculate", "calc") > fuzzy_score("misc", "calc"));
        assert_eq!(fuzzy_score("exact", "EXACT"), Some(20_000));
    }
}
