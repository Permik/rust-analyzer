//! Conversions between [`SyntaxNode`] and [`tt::TokenTree`].

use parser::{ParseError, TreeSink};
use rustc_hash::{FxHashMap, FxHashSet};
use syntax::{
    ast::{self, make::tokens::doc_comment},
    tokenize, AstToken, Parse, PreorderWithTokens, SmolStr, SyntaxElement, SyntaxKind,
    SyntaxKind::*,
    SyntaxNode, SyntaxToken, SyntaxTreeBuilder, TextRange, TextSize, Token as RawToken, WalkEvent,
    T,
};
use tt::buffer::{Cursor, TokenBuffer};

use crate::{
    subtree_source::SubtreeTokenSource, tt_iter::TtIter, ExpandError, ParserEntryPoint, TokenMap,
};

/// Convert the syntax node to a `TokenTree` (what macro
/// will consume).
pub fn syntax_node_to_token_tree(node: &SyntaxNode) -> (tt::Subtree, TokenMap) {
    syntax_node_to_token_tree_censored(node, &Default::default())
}

/// Convert the syntax node to a `TokenTree` (what macro will consume)
/// with the censored range excluded.
pub fn syntax_node_to_token_tree_censored(
    node: &SyntaxNode,
    censor: &FxHashSet<SyntaxNode>,
) -> (tt::Subtree, TokenMap) {
    let global_offset = node.text_range().start();
    let mut c = Convertor::new(node, global_offset, censor);
    let subtree = convert_tokens(&mut c);
    c.id_alloc.map.shrink_to_fit();
    (subtree, c.id_alloc.map)
}

// The following items are what `rustc` macro can be parsed into :
// link: https://github.com/rust-lang/rust/blob/9ebf47851a357faa4cd97f4b1dc7835f6376e639/src/libsyntax/ext/expand.rs#L141
// * Expr(P<ast::Expr>)                     -> token_tree_to_expr
// * Pat(P<ast::Pat>)                       -> token_tree_to_pat
// * Ty(P<ast::Ty>)                         -> token_tree_to_ty
// * Stmts(SmallVec<[ast::Stmt; 1]>)        -> token_tree_to_stmts
// * Items(SmallVec<[P<ast::Item>; 1]>)     -> token_tree_to_items
//
// * TraitItems(SmallVec<[ast::TraitItem; 1]>)
// * AssocItems(SmallVec<[ast::AssocItem; 1]>)
// * ForeignItems(SmallVec<[ast::ForeignItem; 1]>

pub fn token_tree_to_syntax_node(
    tt: &tt::Subtree,
    entry_point: ParserEntryPoint,
) -> Result<(Parse<SyntaxNode>, TokenMap), ExpandError> {
    let buffer = match tt {
        tt::Subtree { delimiter: None, token_trees } => {
            TokenBuffer::from_tokens(token_trees.as_slice())
        }
        _ => TokenBuffer::from_subtree(tt),
    };
    let mut token_source = SubtreeTokenSource::new(&buffer);
    let mut tree_sink = TtTreeSink::new(buffer.begin());
    parser::parse(&mut token_source, &mut tree_sink, entry_point);
    if tree_sink.roots.len() != 1 {
        return Err(ExpandError::ConversionError);
    }
    //FIXME: would be cool to report errors
    let (parse, range_map) = tree_sink.finish();
    Ok((parse, range_map))
}

/// Convert a string to a `TokenTree`
pub fn parse_to_token_tree(text: &str) -> Option<(tt::Subtree, TokenMap)> {
    let (tokens, errors) = tokenize(text);
    if !errors.is_empty() {
        return None;
    }

    let mut conv = RawConvertor {
        text,
        offset: TextSize::default(),
        inner: tokens.iter(),
        id_alloc: TokenIdAlloc {
            map: Default::default(),
            global_offset: TextSize::default(),
            next_id: 0,
        },
    };

    let subtree = convert_tokens(&mut conv);
    Some((subtree, conv.id_alloc.map))
}

/// Split token tree with separate expr: $($e:expr)SEP*
pub fn parse_exprs_with_sep(tt: &tt::Subtree, sep: char) -> Vec<tt::Subtree> {
    if tt.token_trees.is_empty() {
        return Vec::new();
    }

    let mut iter = TtIter::new(tt);
    let mut res = Vec::new();

    while iter.peek_n(0).is_some() {
        let expanded = iter.expect_fragment(ParserEntryPoint::Expr);

        res.push(match expanded.value {
            None => break,
            Some(tt @ tt::TokenTree::Leaf(_)) => {
                tt::Subtree { delimiter: None, token_trees: vec![tt] }
            }
            Some(tt::TokenTree::Subtree(tt)) => tt,
        });

        let mut fork = iter.clone();
        if fork.expect_char(sep).is_err() {
            break;
        }
        iter = fork;
    }

    if iter.peek_n(0).is_some() {
        res.push(tt::Subtree { delimiter: None, token_trees: iter.into_iter().cloned().collect() });
    }

    res
}

fn convert_tokens<C: TokenConvertor>(conv: &mut C) -> tt::Subtree {
    struct StackEntry {
        subtree: tt::Subtree,
        idx: usize,
        open_range: TextRange,
    }

    let entry = StackEntry {
        subtree: tt::Subtree { delimiter: None, ..Default::default() },
        // never used (delimiter is `None`)
        idx: !0,
        open_range: TextRange::empty(TextSize::of('.')),
    };
    let mut stack = vec![entry];

    loop {
        let entry = stack.last_mut().unwrap();
        let result = &mut entry.subtree.token_trees;
        let (token, range) = match conv.bump() {
            None => break,
            Some(it) => it,
        };

        let k: SyntaxKind = token.kind();
        if k == COMMENT {
            if let Some(tokens) = conv.convert_doc_comment(&token) {
                // FIXME: There has to be a better way to do this
                // Add the comments token id to the converted doc string
                let id = conv.id_alloc().alloc(range);
                result.extend(tokens.into_iter().map(|mut tt| {
                    if let tt::TokenTree::Subtree(sub) = &mut tt {
                        if let tt::TokenTree::Leaf(tt::Leaf::Literal(lit)) = &mut sub.token_trees[2]
                        {
                            lit.id = id
                        }
                    }
                    tt
                }));
            }
            continue;
        }

        result.push(if k.is_punct() && k != UNDERSCORE {
            assert_eq!(range.len(), TextSize::of('.'));

            if let Some(delim) = entry.subtree.delimiter {
                let expected = match delim.kind {
                    tt::DelimiterKind::Parenthesis => T![')'],
                    tt::DelimiterKind::Brace => T!['}'],
                    tt::DelimiterKind::Bracket => T![']'],
                };

                if k == expected {
                    let entry = stack.pop().unwrap();
                    conv.id_alloc().close_delim(entry.idx, Some(range));
                    stack.last_mut().unwrap().subtree.token_trees.push(entry.subtree.into());
                    continue;
                }
            }

            let delim = match k {
                T!['('] => Some(tt::DelimiterKind::Parenthesis),
                T!['{'] => Some(tt::DelimiterKind::Brace),
                T!['['] => Some(tt::DelimiterKind::Bracket),
                _ => None,
            };

            if let Some(kind) = delim {
                let mut subtree = tt::Subtree::default();
                let (id, idx) = conv.id_alloc().open_delim(range);
                subtree.delimiter = Some(tt::Delimiter { id, kind });
                stack.push(StackEntry { subtree, idx, open_range: range });
                continue;
            } else {
                let spacing = match conv.peek() {
                    Some(next)
                        if next.kind().is_trivia()
                            || next.kind() == T!['[']
                            || next.kind() == T!['{']
                            || next.kind() == T!['('] =>
                    {
                        tt::Spacing::Alone
                    }
                    Some(next) if next.kind().is_punct() && next.kind() != UNDERSCORE => {
                        tt::Spacing::Joint
                    }
                    _ => tt::Spacing::Alone,
                };
                let char = match token.to_char() {
                    Some(c) => c,
                    None => {
                        panic!("Token from lexer must be single char: token = {:#?}", token);
                    }
                };
                tt::Leaf::from(tt::Punct { char, spacing, id: conv.id_alloc().alloc(range) }).into()
            }
        } else {
            macro_rules! make_leaf {
                ($i:ident) => {
                    tt::$i { id: conv.id_alloc().alloc(range), text: token.to_text() }.into()
                };
            }
            let leaf: tt::Leaf = match k {
                T![true] | T![false] => make_leaf!(Ident),
                IDENT => make_leaf!(Ident),
                UNDERSCORE => make_leaf!(Ident),
                k if k.is_keyword() => make_leaf!(Ident),
                k if k.is_literal() => make_leaf!(Literal),
                LIFETIME_IDENT => {
                    let char_unit = TextSize::of('\'');
                    let r = TextRange::at(range.start(), char_unit);
                    let apostrophe = tt::Leaf::from(tt::Punct {
                        char: '\'',
                        spacing: tt::Spacing::Joint,
                        id: conv.id_alloc().alloc(r),
                    });
                    result.push(apostrophe.into());

                    let r = TextRange::at(range.start() + char_unit, range.len() - char_unit);
                    let ident = tt::Leaf::from(tt::Ident {
                        text: SmolStr::new(&token.to_text()[1..]),
                        id: conv.id_alloc().alloc(r),
                    });
                    result.push(ident.into());
                    continue;
                }
                _ => continue,
            };

            leaf.into()
        });
    }

    // If we get here, we've consumed all input tokens.
    // We might have more than one subtree in the stack, if the delimiters are improperly balanced.
    // Merge them so we're left with one.
    while stack.len() > 1 {
        let entry = stack.pop().unwrap();
        let parent = stack.last_mut().unwrap();

        conv.id_alloc().close_delim(entry.idx, None);
        let leaf: tt::Leaf = tt::Punct {
            id: conv.id_alloc().alloc(entry.open_range),
            char: match entry.subtree.delimiter.unwrap().kind {
                tt::DelimiterKind::Parenthesis => '(',
                tt::DelimiterKind::Brace => '{',
                tt::DelimiterKind::Bracket => '[',
            },
            spacing: tt::Spacing::Alone,
        }
        .into();
        parent.subtree.token_trees.push(leaf.into());
        parent.subtree.token_trees.extend(entry.subtree.token_trees);
    }

    let subtree = stack.pop().unwrap().subtree;
    if subtree.token_trees.len() == 1 {
        if let tt::TokenTree::Subtree(first) = &subtree.token_trees[0] {
            return first.clone();
        }
    }
    subtree
}

/// Returns the textual content of a doc comment block as a quoted string
/// That is, strips leading `///` (or `/**`, etc)
/// and strips the ending `*/`
/// And then quote the string, which is needed to convert to `tt::Literal`
fn doc_comment_text(comment: &ast::Comment) -> SmolStr {
    let prefix_len = comment.prefix().len();
    let mut text = &comment.text()[prefix_len..];

    // Remove ending "*/"
    if comment.kind().shape == ast::CommentShape::Block {
        text = &text[0..text.len() - 2];
    }

    // Quote the string
    // Note that `tt::Literal` expect an escaped string
    let text = format!("\"{}\"", text.escape_debug());
    text.into()
}

fn convert_doc_comment(token: &syntax::SyntaxToken) -> Option<Vec<tt::TokenTree>> {
    cov_mark::hit!(test_meta_doc_comments);
    let comment = ast::Comment::cast(token.clone())?;
    let doc = comment.kind().doc?;

    // Make `doc="\" Comments\""
    let meta_tkns = vec![mk_ident("doc"), mk_punct('='), mk_doc_literal(&comment)];

    // Make `#![]`
    let mut token_trees = vec![mk_punct('#')];
    if let ast::CommentPlacement::Inner = doc {
        token_trees.push(mk_punct('!'));
    }
    token_trees.push(tt::TokenTree::from(tt::Subtree {
        delimiter: Some(tt::Delimiter {
            kind: tt::DelimiterKind::Bracket,
            id: tt::TokenId::unspecified(),
        }),
        token_trees: meta_tkns,
    }));

    return Some(token_trees);

    // Helper functions
    fn mk_ident(s: &str) -> tt::TokenTree {
        tt::TokenTree::from(tt::Leaf::from(tt::Ident {
            text: s.into(),
            id: tt::TokenId::unspecified(),
        }))
    }

    fn mk_punct(c: char) -> tt::TokenTree {
        tt::TokenTree::from(tt::Leaf::from(tt::Punct {
            char: c,
            spacing: tt::Spacing::Alone,
            id: tt::TokenId::unspecified(),
        }))
    }

    fn mk_doc_literal(comment: &ast::Comment) -> tt::TokenTree {
        let lit = tt::Literal { text: doc_comment_text(comment), id: tt::TokenId::unspecified() };

        tt::TokenTree::from(tt::Leaf::from(lit))
    }
}

struct TokenIdAlloc {
    map: TokenMap,
    global_offset: TextSize,
    next_id: u32,
}

impl TokenIdAlloc {
    fn alloc(&mut self, absolute_range: TextRange) -> tt::TokenId {
        let relative_range = absolute_range - self.global_offset;
        let token_id = tt::TokenId(self.next_id);
        self.next_id += 1;
        self.map.insert(token_id, relative_range);
        token_id
    }

    fn open_delim(&mut self, open_abs_range: TextRange) -> (tt::TokenId, usize) {
        let token_id = tt::TokenId(self.next_id);
        self.next_id += 1;
        let idx = self.map.insert_delim(
            token_id,
            open_abs_range - self.global_offset,
            open_abs_range - self.global_offset,
        );
        (token_id, idx)
    }

    fn close_delim(&mut self, idx: usize, close_abs_range: Option<TextRange>) {
        match close_abs_range {
            None => {
                self.map.remove_delim(idx);
            }
            Some(close) => {
                self.map.update_close_delim(idx, close - self.global_offset);
            }
        }
    }
}

/// A Raw Token (straightly from lexer) convertor
struct RawConvertor<'a> {
    text: &'a str,
    offset: TextSize,
    id_alloc: TokenIdAlloc,
    inner: std::slice::Iter<'a, RawToken>,
}

trait SrcToken: std::fmt::Debug {
    fn kind(&self) -> SyntaxKind;

    fn to_char(&self) -> Option<char>;

    fn to_text(&self) -> SmolStr;
}

trait TokenConvertor {
    type Token: SrcToken;

    fn convert_doc_comment(&self, token: &Self::Token) -> Option<Vec<tt::TokenTree>>;

    fn bump(&mut self) -> Option<(Self::Token, TextRange)>;

    fn peek(&self) -> Option<Self::Token>;

    fn id_alloc(&mut self) -> &mut TokenIdAlloc;
}

impl<'a> SrcToken for (&'a RawToken, &'a str) {
    fn kind(&self) -> SyntaxKind {
        self.0.kind
    }

    fn to_char(&self) -> Option<char> {
        self.1.chars().next()
    }

    fn to_text(&self) -> SmolStr {
        self.1.into()
    }
}

impl<'a> TokenConvertor for RawConvertor<'a> {
    type Token = (&'a RawToken, &'a str);

    fn convert_doc_comment(&self, token: &Self::Token) -> Option<Vec<tt::TokenTree>> {
        convert_doc_comment(&doc_comment(token.1))
    }

    fn bump(&mut self) -> Option<(Self::Token, TextRange)> {
        let token = self.inner.next()?;
        let range = TextRange::at(self.offset, token.len);
        self.offset += token.len;

        Some(((token, &self.text[range]), range))
    }

    fn peek(&self) -> Option<Self::Token> {
        let token = self.inner.as_slice().get(0);

        token.map(|it| {
            let range = TextRange::at(self.offset, it.len);
            (it, &self.text[range])
        })
    }

    fn id_alloc(&mut self) -> &mut TokenIdAlloc {
        &mut self.id_alloc
    }
}

struct Convertor<'c> {
    id_alloc: TokenIdAlloc,
    current: Option<SyntaxToken>,
    preorder: PreorderWithTokens,
    censor: &'c FxHashSet<SyntaxNode>,
    range: TextRange,
    punct_offset: Option<(SyntaxToken, TextSize)>,
}

impl<'c> Convertor<'c> {
    fn new(
        node: &SyntaxNode,
        global_offset: TextSize,
        censor: &'c FxHashSet<SyntaxNode>,
    ) -> Convertor<'c> {
        let range = node.text_range();
        let mut preorder = node.preorder_with_tokens();
        let first = Self::next_token(&mut preorder, censor);
        Convertor {
            id_alloc: { TokenIdAlloc { map: TokenMap::default(), global_offset, next_id: 0 } },
            current: first,
            preorder,
            range,
            censor,
            punct_offset: None,
        }
    }

    fn next_token(
        preorder: &mut PreorderWithTokens,
        censor: &FxHashSet<SyntaxNode>,
    ) -> Option<SyntaxToken> {
        while let Some(ev) = preorder.next() {
            let ele = match ev {
                WalkEvent::Enter(ele) => ele,
                _ => continue,
            };
            match ele {
                SyntaxElement::Token(t) => return Some(t),
                SyntaxElement::Node(node) if censor.contains(&node) => preorder.skip_subtree(),
                SyntaxElement::Node(_) => (),
            }
        }
        None
    }
}

#[derive(Debug)]
enum SynToken {
    Ordinary(SyntaxToken),
    Punch(SyntaxToken, TextSize),
}

impl SynToken {
    fn token(&self) -> &SyntaxToken {
        match self {
            SynToken::Ordinary(it) => it,
            SynToken::Punch(it, _) => it,
        }
    }
}

impl SrcToken for SynToken {
    fn kind(&self) -> SyntaxKind {
        self.token().kind()
    }
    fn to_char(&self) -> Option<char> {
        match self {
            SynToken::Ordinary(_) => None,
            SynToken::Punch(it, i) => it.text().chars().nth((*i).into()),
        }
    }
    fn to_text(&self) -> SmolStr {
        self.token().text().into()
    }
}

impl TokenConvertor for Convertor<'_> {
    type Token = SynToken;
    fn convert_doc_comment(&self, token: &Self::Token) -> Option<Vec<tt::TokenTree>> {
        convert_doc_comment(token.token())
    }

    fn bump(&mut self) -> Option<(Self::Token, TextRange)> {
        if let Some((punct, offset)) = self.punct_offset.clone() {
            if usize::from(offset) + 1 < punct.text().len() {
                let offset = offset + TextSize::of('.');
                let range = punct.text_range();
                self.punct_offset = Some((punct.clone(), offset));
                let range = TextRange::at(range.start() + offset, TextSize::of('.'));
                return Some((SynToken::Punch(punct, offset), range));
            }
        }

        let curr = self.current.clone()?;
        if !&self.range.contains_range(curr.text_range()) {
            return None;
        }
        self.current = Self::next_token(&mut self.preorder, self.censor);
        let token = if curr.kind().is_punct() {
            let range = curr.text_range();
            let range = TextRange::at(range.start(), TextSize::of('.'));
            self.punct_offset = Some((curr.clone(), 0.into()));
            (SynToken::Punch(curr, 0.into()), range)
        } else {
            self.punct_offset = None;
            let range = curr.text_range();
            (SynToken::Ordinary(curr), range)
        };

        Some(token)
    }

    fn peek(&self) -> Option<Self::Token> {
        if let Some((punct, mut offset)) = self.punct_offset.clone() {
            offset += TextSize::of('.');
            if usize::from(offset) < punct.text().len() {
                return Some(SynToken::Punch(punct, offset));
            }
        }

        let curr = self.current.clone()?;
        if !self.range.contains_range(curr.text_range()) {
            return None;
        }

        let token = if curr.kind().is_punct() {
            SynToken::Punch(curr, 0.into())
        } else {
            SynToken::Ordinary(curr)
        };
        Some(token)
    }

    fn id_alloc(&mut self) -> &mut TokenIdAlloc {
        &mut self.id_alloc
    }
}

struct TtTreeSink<'a> {
    buf: String,
    cursor: Cursor<'a>,
    open_delims: FxHashMap<tt::TokenId, TextSize>,
    text_pos: TextSize,
    inner: SyntaxTreeBuilder,
    token_map: TokenMap,

    // Number of roots
    // Use for detect ill-form tree which is not single root
    roots: smallvec::SmallVec<[usize; 1]>,
}

impl<'a> TtTreeSink<'a> {
    fn new(cursor: Cursor<'a>) -> Self {
        TtTreeSink {
            buf: String::new(),
            cursor,
            open_delims: FxHashMap::default(),
            text_pos: 0.into(),
            inner: SyntaxTreeBuilder::default(),
            roots: smallvec::SmallVec::new(),
            token_map: TokenMap::default(),
        }
    }

    fn finish(mut self) -> (Parse<SyntaxNode>, TokenMap) {
        self.token_map.shrink_to_fit();
        (self.inner.finish(), self.token_map)
    }
}

fn delim_to_str(d: Option<tt::DelimiterKind>, closing: bool) -> &'static str {
    let texts = match d {
        Some(tt::DelimiterKind::Parenthesis) => "()",
        Some(tt::DelimiterKind::Brace) => "{}",
        Some(tt::DelimiterKind::Bracket) => "[]",
        None => return "",
    };

    let idx = closing as usize;
    &texts[idx..texts.len() - (1 - idx)]
}

impl<'a> TreeSink for TtTreeSink<'a> {
    fn token(&mut self, kind: SyntaxKind, mut n_tokens: u8) {
        if kind == L_DOLLAR || kind == R_DOLLAR {
            self.cursor = self.cursor.bump_subtree();
            return;
        }
        if kind == LIFETIME_IDENT {
            n_tokens = 2;
        }

        let mut last = self.cursor;
        for _ in 0..n_tokens {
            let tmp_str: SmolStr;
            if self.cursor.eof() {
                break;
            }
            last = self.cursor;
            let text: &str = match self.cursor.token_tree() {
                Some(tt::buffer::TokenTreeRef::Leaf(leaf, _)) => {
                    // Mark the range if needed
                    let (text, id) = match leaf {
                        tt::Leaf::Ident(ident) => (&ident.text, ident.id),
                        tt::Leaf::Punct(punct) => {
                            assert!(punct.char.is_ascii());
                            let char = &(punct.char as u8);
                            tmp_str = SmolStr::new_inline(
                                std::str::from_utf8(std::slice::from_ref(char)).unwrap(),
                            );
                            (&tmp_str, punct.id)
                        }
                        tt::Leaf::Literal(lit) => (&lit.text, lit.id),
                    };
                    let range = TextRange::at(self.text_pos, TextSize::of(text.as_str()));
                    self.token_map.insert(id, range);
                    self.cursor = self.cursor.bump();
                    text
                }
                Some(tt::buffer::TokenTreeRef::Subtree(subtree, _)) => {
                    self.cursor = self.cursor.subtree().unwrap();
                    if let Some(id) = subtree.delimiter.map(|it| it.id) {
                        self.open_delims.insert(id, self.text_pos);
                    }
                    delim_to_str(subtree.delimiter_kind(), false)
                }
                None => {
                    if let Some(parent) = self.cursor.end() {
                        self.cursor = self.cursor.bump();
                        if let Some(id) = parent.delimiter.map(|it| it.id) {
                            if let Some(open_delim) = self.open_delims.get(&id) {
                                let open_range = TextRange::at(*open_delim, TextSize::of('('));
                                let close_range = TextRange::at(self.text_pos, TextSize::of('('));
                                self.token_map.insert_delim(id, open_range, close_range);
                            }
                        }
                        delim_to_str(parent.delimiter_kind(), true)
                    } else {
                        continue;
                    }
                }
            };
            self.buf += text;
            self.text_pos += TextSize::of(text);
        }

        self.inner.token(kind, self.buf.as_str());
        self.buf.clear();
        // Add whitespace between adjoint puncts
        let next = last.bump();
        if let (
            Some(tt::buffer::TokenTreeRef::Leaf(tt::Leaf::Punct(curr), _)),
            Some(tt::buffer::TokenTreeRef::Leaf(tt::Leaf::Punct(_), _)),
        ) = (last.token_tree(), next.token_tree())
        {
            // Note: We always assume the semi-colon would be the last token in
            // other parts of RA such that we don't add whitespace here.
            if curr.spacing == tt::Spacing::Alone && curr.char != ';' {
                self.inner.token(WHITESPACE, " ");
                self.text_pos += TextSize::of(' ');
            }
        }
    }

    fn start_node(&mut self, kind: SyntaxKind) {
        self.inner.start_node(kind);

        match self.roots.last_mut() {
            None | Some(0) => self.roots.push(1),
            Some(ref mut n) => **n += 1,
        };
    }

    fn finish_node(&mut self) {
        self.inner.finish_node();
        *self.roots.last_mut().unwrap() -= 1;
    }

    fn error(&mut self, error: ParseError) {
        self.inner.error(error, self.text_pos)
    }
}
