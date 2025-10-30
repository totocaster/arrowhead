use std::{collections::BTreeMap, fmt};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};

use super::time::{
    DateRange, DateRangeBound, parse_absolute_date, parse_relative_range, range_from_lower,
    range_from_parsed_date, range_from_upper,
};

/// Fully parsed query including the FTS expression and structured filters.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedQuery {
    /// Normalised SQLite FTS query string (absent when the query is filter-only).
    pub fts: Option<String>,
    /// FTS expressions that should be excluded from the result set.
    pub excludes: Vec<String>,
    /// Structured filter constraints (filesystem timestamps, metadata dates).
    pub filters: QueryFilters,
}

/// Structured filters extracted from the query.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct QueryFilters {
    /// Filesystem modified timestamp range.
    pub modified: Option<DateRange>,
    /// Filesystem created timestamp range.
    pub created: Option<DateRange>,
    /// Front-matter (metadata) date ranges keyed by field name.
    pub metadata_dates: BTreeMap<String, DateRange>,
}

impl QueryFilters {
    fn apply_modified(&mut self, range: DateRange) -> Result<()> {
        self.modified = merge_ranges(self.modified.take(), range)?;
        Ok(())
    }

    fn apply_created(&mut self, range: DateRange) -> Result<()> {
        self.created = merge_ranges(self.created.take(), range)?;
        Ok(())
    }

    fn apply_metadata(&mut self, key: &str, range: DateRange) -> Result<()> {
        let entry = self.metadata_dates.remove(key);
        let merged = merge_ranges(entry, range)?;
        if let Some(range) = merged {
            self.metadata_dates.insert(key.to_string(), range);
        }
        Ok(())
    }

    /// Returns `true` when no filters are active.
    pub fn is_empty(&self) -> bool {
        self.modified.is_none() && self.created.is_none() && self.metadata_dates.is_empty()
    }

    /// Count the number of active filter buckets (filesystem + metadata fields).
    pub fn active_count(&self) -> usize {
        usize::from(self.modified.is_some())
            + usize::from(self.created.is_some())
            + self.metadata_dates.len()
    }
}

fn merge_ranges(existing: Option<DateRange>, new_range: DateRange) -> Result<Option<DateRange>> {
    if new_range.is_empty() {
        bail!("date range is empty");
    }

    if let Some(current) = existing {
        if let Some(intersection) = current.intersect(&new_range) {
            if intersection.is_empty() {
                bail!("date range filters exclude one another");
            }
            Ok(Some(intersection))
        } else {
            bail!("date range filters exclude one another");
        }
    } else {
        Ok(Some(new_range))
    }
}

/// Parse a query string using the current UTC timestamp as the relative-date anchor.
pub fn parse_query(input: &str) -> Result<ParsedQuery> {
    parse_query_with_now(input, Utc::now())
}

/// Parse a query string using the provided reference time (useful for tests).
pub fn parse_query_with_now(input: &str, now: DateTime<Utc>) -> Result<ParsedQuery> {
    let mut lexer = Lexer::new(input);
    let tokens = lexer.lex()?;
    let mut parser = Parser::new(tokens, now);
    let expr = parser.parse_expression()?;
    parser.expect(TokenKind::Eof)?;

    let mut filters = QueryFilters::default();
    let fts_expr = strip_filters(expr, &mut filters)?;
    let mut excludes = Vec::new();
    let fts = match fts_expr {
        Some(expr) => {
            let (positive, negative) = split_negations(expr)?;
            excludes = negative
                .into_iter()
                .map(|neg| build_fts(&neg))
                .collect::<Result<Vec<_>>>()?;
            positive.map(|pos| build_fts(&pos)).transpose()?
        }
        None => None,
    };

    Ok(ParsedQuery {
        fts,
        excludes,
        filters,
    })
}

#[derive(Debug, Clone, PartialEq)]
enum Expr {
    Term(FtsTerm),
    Filter(FilterKind),
    And(Vec<Expr>),
    Or(Vec<Expr>),
    Not(Box<Expr>),
}

#[derive(Debug, Clone, PartialEq)]
enum FilterKind {
    Modified(DateRange),
    Created(DateRange),
    MetadataDate { field: String, range: DateRange },
}

#[derive(Debug, Clone, PartialEq)]
enum FtsTerm {
    Default(ValueToken),
    Content(ValueToken),
    Metadata { field: String, value: ValueToken },
}

#[derive(Debug, Clone, PartialEq)]
enum ValueToken {
    Word(String),
    Quoted(String),
}

impl ValueToken {
    fn as_str(&self) -> &str {
        match self {
            ValueToken::Word(value) => value,
            ValueToken::Quoted(value) => value,
        }
    }

    fn is_quoted(&self) -> bool {
        matches!(self, ValueToken::Quoted(_))
    }
}

fn strip_filters(expr: Expr, filters: &mut QueryFilters) -> Result<Option<Expr>> {
    match expr {
        Expr::Filter(FilterKind::Modified(range)) => {
            filters.apply_modified(range)?;
            Ok(None)
        }
        Expr::Filter(FilterKind::Created(range)) => {
            filters.apply_created(range)?;
            Ok(None)
        }
        Expr::Filter(FilterKind::MetadataDate { field, range }) => {
            filters.apply_metadata(&field, range)?;
            Ok(None)
        }
        Expr::Term(term) => Ok(Some(Expr::Term(term))),
        Expr::Not(inner) => {
            let inner =
                strip_filters(*inner, filters)?.context("cannot apply NOT to date filters")?;
            Ok(Some(Expr::Not(Box::new(inner))))
        }
        Expr::And(items) => {
            let mut kept = Vec::new();
            for item in items {
                if let Some(expr) = strip_filters(item, filters)? {
                    kept.push(expr);
                }
            }
            if kept.is_empty() {
                Ok(None)
            } else if kept.len() == 1 {
                Ok(Some(kept.into_iter().next().unwrap()))
            } else {
                Ok(Some(Expr::And(kept)))
            }
        }
        Expr::Or(items) => {
            let mut kept = Vec::new();
            for item in items {
                match strip_filters(item, filters)? {
                    Some(expr) => kept.push(expr),
                    None => bail!("date filters must be combined with AND operators"),
                }
            }
            if kept.is_empty() {
                Ok(None)
            } else if kept.len() == 1 {
                Ok(Some(kept.into_iter().next().unwrap()))
            } else {
                Ok(Some(Expr::Or(kept)))
            }
        }
    }
}

fn split_negations(expr: Expr) -> Result<(Option<Expr>, Vec<Expr>)> {
    match expr {
        Expr::Term(term) => Ok((Some(Expr::Term(term)), Vec::new())),
        Expr::Not(inner) => Ok((None, vec![*inner])),
        Expr::And(items) => {
            let mut positives = Vec::new();
            let mut negatives = Vec::new();
            for item in items {
                let (pos, mut negs) = split_negations(item)?;
                if let Some(pos_expr) = pos {
                    positives.push(pos_expr);
                }
                negatives.append(&mut negs);
            }
            let positive_expr = match positives.len() {
                0 => None,
                1 => Some(positives.pop().unwrap()),
                _ => Some(Expr::And(positives)),
            };
            Ok((positive_expr, negatives))
        }
        Expr::Or(items) => {
            let mut transformed = Vec::new();
            for item in items {
                let (pos, negs) = split_negations(item)?;
                if !negs.is_empty() {
                    bail!("NOT operator is only supported within AND expressions");
                }
                if let Some(pos_expr) = pos {
                    transformed.push(pos_expr);
                } else {
                    bail!("NOT operator cannot stand alone inside OR expressions");
                }
            }
            let expr = match transformed.len() {
                0 => None,
                1 => Some(transformed.pop().unwrap()),
                _ => Some(Expr::Or(transformed)),
            };
            Ok((expr, Vec::new()))
        }
        Expr::Filter(_) => unreachable!("filters removed before split_negations"),
    }
}

fn build_fts(expr: &Expr) -> Result<String> {
    fn precedence(expr: &Expr) -> u8 {
        match expr {
            Expr::Or(_) => 1,
            Expr::And(_) => 2,
            Expr::Not(_) => 3,
            Expr::Term(_) => 4,
            Expr::Filter(_) => 0,
        }
    }

    fn render(expr: &Expr, parent_prec: u8) -> Result<String> {
        let prec = precedence(expr);
        let result = match expr {
            Expr::Term(term) => term.to_fts(),
            Expr::Not(_) => bail!("NOT expressions must be handled before rendering"),
            Expr::And(items) => {
                let mut parts = Vec::new();
                for child in items {
                    parts.push(render(child, prec)?);
                }
                parts.join(" AND ")
            }
            Expr::Or(items) => {
                let mut parts = Vec::new();
                for child in items {
                    parts.push(render(child, prec)?);
                }
                parts.join(" OR ")
            }
            Expr::Filter(_) => String::new(),
        };

        if prec < parent_prec {
            Ok(format!("({result})"))
        } else {
            Ok(result)
        }
    }

    render(expr, 0)
}

impl FtsTerm {
    fn to_fts(&self) -> String {
        match self {
            FtsTerm::Default(value) => format!("{{content metadata}} : {}", render_value(value)),
            FtsTerm::Content(value) => format!("content:{}", render_value(value)),
            FtsTerm::Metadata { field, value } => {
                let token = format!("{field}:{}", value.as_str());
                let escaped = escape_metadata_token(&token);
                format!("metadata:{escaped}")
            }
        }
    }
}

fn render_value(value: &ValueToken) -> String {
    if value.is_quoted() {
        let escaped = value.as_str().replace('"', "\"\"");
        format!("\"{escaped}\"")
    } else {
        escape_fts_literal(value.as_str())
    }
}

fn escape_metadata_token(token: &str) -> String {
    let escaped = token.replace('"', "\"\"");
    format!("\"{escaped}\"")
}

fn escape_fts_literal(value: &str) -> String {
    if value.is_empty() {
        return String::new();
    }

    if value.starts_with('"') && value.ends_with('"') && value.len() >= 2 {
        return value.to_string();
    }

    if requires_quotes(value) {
        let escaped = value.replace('"', "\"\"");
        format!("\"{escaped}\"")
    } else {
        value.to_string()
    }
}

fn requires_quotes(value: &str) -> bool {
    static HYPHEN_PATTERN: once_cell::sync::Lazy<regex::Regex> = once_cell::sync::Lazy::new(|| {
        regex::Regex::new(r"\b[\w]+-[\w-]+\b").expect("valid hyphen regex")
    });

    let trimmed = value.trim();
    if trimmed.is_empty() {
        return false;
    }

    if HYPHEN_PATTERN.is_match(trimmed) {
        return true;
    }

    trimmed
        .chars()
        .any(|ch| matches!(ch, '[' | ']' | '(' | ')' | '"' | '*'))
        || trimmed.starts_with('-')
        || trimmed.contains(" AND ")
        || trimmed.contains(" OR ")
        || trimmed.contains(" NOT ")
}

// ===== Lexer =================================================================

#[derive(Debug, Clone, PartialEq)]
struct Token {
    kind: TokenKind,
    lexeme: String,
}

#[derive(Debug, Clone, PartialEq)]
enum TokenKind {
    Word,
    String,
    And,
    Or,
    Not,
    LParen,
    RParen,
    Colon,
    Range,
    Ge,
    Le,
    Gt,
    Lt,
    Eof,
}

impl fmt::Display for TokenKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self)
    }
}

struct Lexer<'a> {
    input: &'a str,
    position: usize,
}

impl<'a> Lexer<'a> {
    fn new(input: &'a str) -> Self {
        Self { input, position: 0 }
    }

    fn lex(&mut self) -> Result<Vec<Token>> {
        let mut tokens = Vec::new();
        loop {
            let token = self.next_token()?;
            let end = matches!(token.kind, TokenKind::Eof);
            tokens.push(token);
            if end {
                break;
            }
        }
        Ok(tokens)
    }

    fn next_token(&mut self) -> Result<Token> {
        self.skip_whitespace();
        if self.is_eof() {
            return Ok(Token {
                kind: TokenKind::Eof,
                lexeme: String::new(),
            });
        }

        let ch = self.peek_char().unwrap();
        match ch {
            '(' => {
                self.advance_char();
                Ok(Token {
                    kind: TokenKind::LParen,
                    lexeme: "(".to_string(),
                })
            }
            ')' => {
                self.advance_char();
                Ok(Token {
                    kind: TokenKind::RParen,
                    lexeme: ")".to_string(),
                })
            }
            ':' => {
                self.advance_char();
                Ok(Token {
                    kind: TokenKind::Colon,
                    lexeme: ":".to_string(),
                })
            }
            '"' => self.lex_string(),
            '>' => {
                self.advance_char();
                if self.peek_char() == Some('=') {
                    self.advance_char();
                    Ok(Token {
                        kind: TokenKind::Ge,
                        lexeme: ">=".to_string(),
                    })
                } else {
                    Ok(Token {
                        kind: TokenKind::Gt,
                        lexeme: ">".to_string(),
                    })
                }
            }
            '<' => {
                self.advance_char();
                if self.peek_char() == Some('=') {
                    self.advance_char();
                    Ok(Token {
                        kind: TokenKind::Le,
                        lexeme: "<=".to_string(),
                    })
                } else {
                    Ok(Token {
                        kind: TokenKind::Lt,
                        lexeme: "<".to_string(),
                    })
                }
            }
            '.' => {
                if self.peek_two_dots() {
                    self.advance_char();
                    self.advance_char();
                    Ok(Token {
                        kind: TokenKind::Range,
                        lexeme: "..".to_string(),
                    })
                } else {
                    self.lex_word()
                }
            }
            _ => self.lex_word(),
        }
    }

    fn lex_string(&mut self) -> Result<Token> {
        self.advance_char(); // consume opening quote
        let mut value = String::new();
        let mut escaped = false;
        while let Some(ch) = self.advance_char() {
            if escaped {
                value.push(ch);
                escaped = false;
                continue;
            }
            match ch {
                '\\' => escaped = true,
                '"' => {
                    return Ok(Token {
                        kind: TokenKind::String,
                        lexeme: value,
                    });
                }
                _ => value.push(ch),
            }
        }
        bail!("unterminated string literal");
    }

    fn lex_word(&mut self) -> Result<Token> {
        let mut buffer = String::new();
        while let Some(ch) = self.peek_char() {
            if ch.is_whitespace() || matches!(ch, '(' | ')' | ':' | '"' | '<' | '>') {
                break;
            }
            if ch == '.' && self.peek_two_dots() {
                break;
            }
            buffer.push(ch);
            self.advance_char();
        }

        if buffer.is_empty() {
            bail!("unexpected character at position {}", self.position);
        }

        let upper = buffer.to_ascii_uppercase();
        if upper == "AND" {
            return Ok(Token {
                kind: TokenKind::And,
                lexeme: buffer,
            });
        }
        if upper == "OR" {
            return Ok(Token {
                kind: TokenKind::Or,
                lexeme: buffer,
            });
        }
        if upper == "NOT" {
            return Ok(Token {
                kind: TokenKind::Not,
                lexeme: buffer,
            });
        }

        Ok(Token {
            kind: TokenKind::Word,
            lexeme: buffer,
        })
    }

    fn skip_whitespace(&mut self) {
        while let Some(ch) = self.peek_char() {
            if ch.is_whitespace() {
                self.advance_char();
            } else {
                break;
            }
        }
    }

    fn peek_char(&self) -> Option<char> {
        self.input[self.position..].chars().next()
    }

    fn advance_char(&mut self) -> Option<char> {
        let ch = self.peek_char()?;
        self.position += ch.len_utf8();
        Some(ch)
    }

    fn peek_two_dots(&self) -> bool {
        let mut chars = self.input[self.position..].chars();
        matches!(chars.next(), Some('.')) && matches!(chars.next(), Some('.'))
    }

    fn is_eof(&self) -> bool {
        self.position >= self.input.len()
    }
}

// ===== Parser ================================================================

struct Parser {
    tokens: Vec<Token>,
    index: usize,
    now: DateTime<Utc>,
}

impl Parser {
    fn new(tokens: Vec<Token>, now: DateTime<Utc>) -> Self {
        Self {
            tokens,
            index: 0,
            now,
        }
    }

    fn parse_expression(&mut self) -> Result<Expr> {
        self.parse_or()
    }

    fn parse_or(&mut self) -> Result<Expr> {
        let mut expr = self.parse_and()?;
        while self.matches(TokenKind::Or) {
            let rhs = self.parse_and()?;
            expr = match expr {
                Expr::Or(mut items) => {
                    items.push(rhs);
                    Expr::Or(items)
                }
                other => Expr::Or(vec![other, rhs]),
            };
        }
        Ok(expr)
    }

    fn parse_and(&mut self) -> Result<Expr> {
        let mut exprs = vec![self.parse_unary()?];
        loop {
            if self.matches(TokenKind::And) {
                exprs.push(self.parse_unary()?);
            } else if self.is_start_of_primary() {
                exprs.push(self.parse_unary()?);
            } else {
                break;
            }
        }

        if exprs.len() == 1 {
            Ok(exprs.into_iter().next().unwrap())
        } else {
            Ok(Expr::And(exprs))
        }
    }

    fn parse_unary(&mut self) -> Result<Expr> {
        if self.matches(TokenKind::Not) {
            let expr = self.parse_unary()?;
            Ok(Expr::Not(Box::new(expr)))
        } else {
            self.parse_primary()
        }
    }

    fn parse_primary(&mut self) -> Result<Expr> {
        if self.matches(TokenKind::LParen) {
            let expr = self.parse_expression()?;
            self.expect(TokenKind::RParen)?;
            return Ok(expr);
        }

        match self.peek_kind() {
            Some(TokenKind::Word) => self.parse_word_prefixed(),
            Some(TokenKind::String) => {
                let token = self.advance().clone();
                Ok(Expr::Term(FtsTerm::Default(ValueToken::Quoted(
                    token.lexeme,
                ))))
            }
            Some(TokenKind::Range) => {
                bail!("unexpected range operator; specify a field before `..`")
            }
            Some(TokenKind::Colon) => {
                bail!("field name required before `:`")
            }
            Some(other) => bail!("unexpected token: {other:?}"),
            None => bail!("unexpected end of input"),
        }
    }

    fn parse_word_prefixed(&mut self) -> Result<Expr> {
        let token = self.advance().clone();
        let word = token.lexeme.clone();
        if self.matches(TokenKind::Colon) {
            self.parse_field_expression(word)
        } else {
            Ok(Expr::Term(FtsTerm::Default(ValueToken::Word(word))))
        }
    }

    fn parse_field_expression(&mut self, field: String) -> Result<Expr> {
        use TokenKind::*;
        let lowered = field.to_ascii_lowercase();

        let canonical_field = resolve_field_alias(&lowered);

        if is_date_filter_field(canonical_field) {
            let range = self.parse_date_filter(canonical_field)?;
            let kind = match canonical_field {
                "modified" => FilterKind::Modified(range),
                "created" => FilterKind::Created(range),
                other => FilterKind::MetadataDate {
                    field: other.to_string(),
                    range,
                },
            };
            return Ok(Expr::Filter(kind));
        }

        if self.matches(TokenKind::Range) {
            bail!("range queries require date-compatible fields (found `{field}`)");
        }

        let value = match self.peek_kind() {
            Some(String) => ValueToken::Quoted(self.advance().lexeme.clone()),
            Some(Word) => ValueToken::Word(self.advance().lexeme.clone()),
            Some(Ge | Le | Gt | Lt) => {
                let comparator = self.advance().lexeme.clone();
                let rhs = self.parse_value_token()?;
                let raw = format!("{comparator}{}", rhs.as_str());
                ValueToken::Word(raw)
            }
            Some(Range) => bail!("range queries require date-compatible fields"),
            Some(kind) => bail!("unexpected token after `{field}:` ({kind:?})"),
            None => bail!("incomplete field expression after `{field}:`"),
        };

        if canonical_field == "content" {
            Ok(Expr::Term(FtsTerm::Content(value)))
        } else {
            Ok(Expr::Term(FtsTerm::Metadata {
                field: canonical_field.to_string(),
                value,
            }))
        }
    }

    fn parse_date_filter(&mut self, field: &str) -> Result<DateRange> {
        if self.matches(TokenKind::Range) {
            let end = self.parse_optional_date_operand(true)?;
            if end.is_none() {
                bail!("open range `..` must include an upper bound");
            }
            return Ok(DateRange::new(None, Some(end.unwrap())));
        }

        if self.matches(TokenKind::Ge) || self.matches(TokenKind::Gt) {
            let inclusive = self.previous().kind == TokenKind::Ge;
            let bound = self.parse_date_operand(inclusive)?;
            return Ok(range_from_lower(bound));
        }

        if self.matches(TokenKind::Le) || self.matches(TokenKind::Lt) {
            let inclusive = self.previous().kind == TokenKind::Le;
            let bound = self.parse_date_operand(inclusive)?;
            return Ok(range_from_upper(bound));
        }

        let first = self.parse_value_token()?;

        if self.matches(TokenKind::Range) {
            let end = self.parse_optional_date_operand(true)?;
            let start = self.convert_value_to_range_bound(first.as_str(), true)?;
            let range = match end {
                Some(bound) => DateRange::new(Some(start), Some(bound)),
                None => DateRange::new(Some(start), None),
            };
            return Ok(range);
        }

        if let Some(range) = parse_relative_range(first.as_str(), self.now)? {
            return Ok(range);
        }

        let parsed = parse_absolute_date(first.as_str())
            .with_context(|| format!("invalid date literal for `{field}`"))?;
        Ok(range_from_parsed_date(parsed))
    }

    fn parse_optional_date_operand(&mut self, inclusive: bool) -> Result<Option<DateRangeBound>> {
        if matches!(
            self.peek_kind(),
            Some(TokenKind::Eof | TokenKind::RParen | TokenKind::And | TokenKind::Or)
        ) {
            return Ok(None);
        }
        let token = self.parse_value_token()?;
        self.convert_value_to_range_bound(token.as_str(), inclusive)
            .map(Some)
    }

    fn parse_date_operand(&mut self, inclusive: bool) -> Result<DateRangeBound> {
        let token = self.parse_value_token()?;
        self.convert_value_to_range_bound(token.as_str(), inclusive)
    }

    fn convert_value_to_range_bound(&self, value: &str, inclusive: bool) -> Result<DateRangeBound> {
        if let Some(range) = parse_relative_range(value, self.now)? {
            let start = range.start;
            let end = range.end;
            let chosen = if inclusive {
                end.or(start)
                    .context("relative date range missing upper bound")?
            } else {
                start
                    .or(end)
                    .context("relative date range missing lower bound")?
            };
            return Ok(DateRangeBound {
                value: chosen.value,
                inclusive,
            });
        }

        let parsed = parse_absolute_date(value)?;
        Ok(DateRangeBound {
            value: parsed.instant,
            inclusive,
        })
    }

    fn parse_value_token(&mut self) -> Result<ValueToken> {
        match self.peek_kind() {
            Some(TokenKind::String) => Ok(ValueToken::Quoted(self.advance().lexeme.clone())),
            Some(TokenKind::Word) => Ok(ValueToken::Word(self.advance().lexeme.clone())),
            other => bail!("expected value, found {other:?}"),
        }
    }

    fn is_start_of_primary(&self) -> bool {
        matches!(
            self.peek_kind(),
            Some(TokenKind::Word | TokenKind::String | TokenKind::LParen | TokenKind::Not)
        )
    }

    fn matches(&mut self, kind: TokenKind) -> bool {
        if self.peek_kind() == Some(kind.clone()) {
            self.advance();
            true
        } else {
            false
        }
    }

    fn expect(&mut self, kind: TokenKind) -> Result<()> {
        if self.matches(kind.clone()) {
            Ok(())
        } else {
            bail!("expected token {kind:?}")
        }
    }

    fn advance(&mut self) -> &Token {
        let token = &self.tokens[self.index];
        self.index += 1;
        token
    }

    fn previous(&self) -> &Token {
        &self.tokens[self.index - 1]
    }

    fn peek_kind(&self) -> Option<TokenKind> {
        self.tokens.get(self.index).map(|token| token.kind.clone())
    }
}

fn is_date_filter_field(field: &str) -> bool {
    matches!(field, "modified" | "created" | "date" | "review_due")
}

fn resolve_field_alias(field: &str) -> &str {
    match field {
        "m" => "modified",
        "c" => "created",
        other => other,
    }
}

// ===== Tests =================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn parse(query: &str) -> ParsedQuery {
        let now = Utc.with_ymd_and_hms(2024, 5, 10, 12, 0, 0).unwrap();
        parse_query_with_now(query, now).expect("query parses")
    }

    #[test]
    fn parses_basic_boolean_expression() {
        let parsed = parse("alpha AND beta OR gamma");
        assert_eq!(
            parsed.fts.unwrap(),
            "{content metadata} : alpha AND {content metadata} : beta OR {content metadata} : gamma"
        );
    }

    #[test]
    fn parses_metadata_field() {
        let parsed = parse("tags:rust");
        assert_eq!(parsed.fts.unwrap(), "metadata:\"tags:rust\"");
    }

    #[test]
    fn parses_content_field() {
        let parsed = parse("content:\"status update\"");
        assert_eq!(parsed.fts.unwrap(), "content:\"status update\"");
    }

    #[test]
    fn implicit_and_handled() {
        let parsed = parse("project status");
        assert_eq!(
            parsed.fts.unwrap(),
            "{content metadata} : project AND {content metadata} : status"
        );
    }

    #[test]
    fn grouping_respected() {
        let parsed = parse("alpha OR (beta AND gamma)");
        assert_eq!(
            parsed.fts.unwrap(),
            "{content metadata} : alpha OR {content metadata} : beta AND {content metadata} : gamma"
        );
    }

    #[test]
    fn not_operator_populates_excludes() {
        let parsed = parse("photography NOT digital");
        assert_eq!(parsed.fts.unwrap(), "{content metadata} : photography");
        assert_eq!(parsed.excludes.len(), 1);
        assert_eq!(parsed.excludes[0], "{content metadata} : digital");
    }

    #[test]
    fn not_inside_or_errors() {
        let now = Utc.with_ymd_and_hms(2024, 5, 10, 12, 0, 0).unwrap();
        let err = parse_query_with_now("alpha OR NOT beta", now)
            .expect_err("should reject NOT inside OR");
        assert!(
            err.to_string()
                .contains("NOT operator is only supported within AND expressions")
        );
    }

    #[test]
    fn extracts_modified_range() {
        let parsed = parse("modified:2024-05-01..2024-05-31 AND status:active");
        let fts = parsed.fts.unwrap();
        assert!(fts.contains("metadata:\"status:active\""));
        let range = parsed.filters.modified.as_ref().unwrap();
        assert!(range.start.is_some());
        assert!(range.end.is_some());
    }

    #[test]
    fn open_ended_date_ranges_supported() {
        let parsed = parse("created:2024-05-01..");
        assert!(parsed.filters.created.as_ref().unwrap().end.is_none());
    }

    #[test]
    fn leading_open_range_supported() {
        let parsed = parse("created:..2024-05-15");
        assert!(parsed.filters.created.as_ref().unwrap().start.is_none());
        assert!(parsed.filters.created.as_ref().unwrap().end.is_some());
    }

    #[test]
    fn relative_date_tokens_supported() {
        let parsed = parse("modified:past7d");
        assert!(parsed.fts.is_none());
        assert!(parsed.filters.modified.is_some());
    }

    #[test]
    fn field_aliases_are_resolved() {
        let parsed = parse("m:past7d AND c:>=2024-05-01");
        assert!(parsed.filters.modified.is_some());
        assert!(parsed.filters.created.is_some());
    }

    #[test]
    fn named_relative_ranges_supported() {
        let parsed = parse("modified:today");
        assert!(parsed.filters.modified.is_some());
        let another = parse("created:lastweek");
        assert!(another.filters.created.is_some());
    }

    #[test]
    fn metadata_date_filters_recorded() {
        let parsed = parse("date:2024-05-01");
        assert!(parsed.fts.is_none());
        assert!(parsed.filters.metadata_dates.contains_key("date"));
    }

    #[test]
    fn rejects_date_filter_in_or() {
        let now = Utc.with_ymd_and_hms(2024, 5, 10, 12, 0, 0).unwrap();
        let err = parse_query_with_now("modified:past7d OR tags:rust", now).unwrap_err();
        assert!(
            err.to_string()
                .contains("date filters must be combined with AND")
        );
    }

    #[test]
    fn rejects_negated_date_filter() {
        let now = Utc.with_ymd_and_hms(2024, 5, 10, 12, 0, 0).unwrap();
        let err = parse_query_with_now("NOT modified:past7d", now).unwrap_err();
        assert!(err.to_string().contains("cannot apply NOT to date filters"));
    }

    #[test]
    fn non_date_range_errors() {
        let now = Utc.with_ymd_and_hms(2024, 5, 10, 12, 0, 0).unwrap();
        let err = parse_query_with_now("title:..2024", now).unwrap_err();
        assert!(
            err.to_string()
                .contains("range queries require date-compatible fields")
        );
    }

    #[test]
    fn quoted_metadata_preserved() {
        let parsed = parse("aliases:\"Launch Plan\"");
        assert_eq!(parsed.fts.unwrap(), "metadata:\"aliases:Launch Plan\"");
    }

    #[test]
    fn merges_multiple_date_filters() {
        let parsed = parse("modified:>=2024-05-01 AND modified:<=2024-05-31");
        assert!(parsed.fts.is_none());
        let range = parsed.filters.modified.as_ref().unwrap();
        assert!(range.start.is_some());
        assert!(range.end.is_some());
    }
}
