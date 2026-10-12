//! Line-by-line port of htmlparser2 8.0.2 `lib/esm/Tokenizer.js` for the
//! default HTML mode (`xmlMode: false`, `decodeEntities: true`).
//!
//! The original walks UTF-16 code units. Every character the state machine
//! inspects is ASCII and no ASCII comparison can match a UTF-8 lead or
//! continuation byte, so walking UTF-8 bytes produces the same events at the
//! same character boundaries. The whole document is kept in one slice;
//! `write(end)` makes the bytes up to `end` available, which matches
//! htmlparser2's chunked `write` calls.

use super::entities::{HTML_DECODE_TREE, VALUE_LENGTH, determine_branch, replace_code_point, tree_at};

const TAB: u8 = 9;
const NEW_LINE: u8 = 10;
const FORM_FEED: u8 = 12;
const CARRIAGE_RETURN: u8 = 13;
const SPACE: u8 = 32;
const EXCLAMATION_MARK: u8 = 33;
const NUMBER: u8 = 35;
const AMP: u8 = 38;
const SINGLE_QUOTE: u8 = 39;
const DOUBLE_QUOTE: u8 = 34;
const DASH: u8 = 45;
const SLASH: u8 = 47;
const ZERO: u8 = 48;
const NINE: u8 = 57;
const SEMI: u8 = 59;
const LT: u8 = 60;
const EQ: u8 = 61;
const GT: u8 = 62;
const QUESTIONMARK: u8 = 63;
const UPPER_A: u8 = 65;
const LOWER_A: u8 = 97;
const UPPER_F: u8 = 70;
const LOWER_F: u8 = 102;
const UPPER_Z: u8 = 90;
const LOWER_Z: u8 = 122;
const LOWER_X: u8 = 120;
const OPENING_SQUARE_BRACKET: u8 = 91;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    Text,
    BeforeTagName,
    InTagName,
    InSelfClosingTag,
    BeforeClosingTagName,
    InClosingTagName,
    AfterClosingTagName,
    BeforeAttributeName,
    InAttributeName,
    AfterAttributeName,
    BeforeAttributeValue,
    InAttributeValueDq,
    InAttributeValueSq,
    InAttributeValueNq,
    BeforeDeclaration,
    InDeclaration,
    InProcessingInstruction,
    BeforeComment,
    CdataSequence,
    InSpecialComment,
    InCommentLike,
    BeforeSpecialS,
    SpecialStartSequence,
    InSpecialTag,
    BeforeEntity,
    BeforeNumericEntity,
    InNamedEntity,
    InNumericEntity,
    InHexEntity,
}

fn is_whitespace(c: u8) -> bool {
    c == SPACE || c == NEW_LINE || c == TAB || c == FORM_FEED || c == CARRIAGE_RETURN
}
fn is_end_of_tag_section(c: u8) -> bool {
    c == SLASH || c == GT || is_whitespace(c)
}
fn is_number(c: u8) -> bool {
    (ZERO..=NINE).contains(&c)
}
fn is_ascii_alpha(c: u8) -> bool {
    (LOWER_A..=LOWER_Z).contains(&c) || (UPPER_A..=UPPER_Z).contains(&c)
}
fn is_hex_digit(c: u8) -> bool {
    (UPPER_A..=UPPER_F).contains(&c) || (LOWER_A..=LOWER_F).contains(&c)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum QuoteType {
    NoValue,
    Unquoted,
    Single,
    Double,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Sequence {
    Cdata,
    CdataEnd,
    CommentEnd,
    ScriptEnd,
    StyleEnd,
    TitleEnd,
}

impl Sequence {
    fn bytes(self) -> &'static [u8] {
        match self {
            Sequence::Cdata => &[0x43, 0x44, 0x41, 0x54, 0x41, 0x5b],
            Sequence::CdataEnd => &[0x5d, 0x5d, 0x3e],
            Sequence::CommentEnd => &[0x2d, 0x2d, 0x3e],
            Sequence::ScriptEnd => &[0x3c, 0x2f, 0x73, 0x63, 0x72, 0x69, 0x70, 0x74],
            Sequence::StyleEnd => &[0x3c, 0x2f, 0x73, 0x74, 0x79, 0x6c, 0x65],
            Sequence::TitleEnd => &[0x3c, 0x2f, 0x74, 0x69, 0x74, 0x6c, 0x65],
        }
    }
}

/// Callbacks the tokenizer invokes (htmlparser2 `Callbacks`).
pub(super) trait TokenSink {
    fn input(&self) -> &[u8];
    fn ontext(&mut self, start: isize, end: isize);
    fn ontextentity(&mut self, cp: u32);
    fn onopentagname(&mut self, start: isize, end: isize);
    fn onopentagend(&mut self, end: isize);
    fn onselfclosingtag(&mut self, end: isize);
    fn onclosetag(&mut self, start: isize, end: isize);
    fn onattribname(&mut self, start: isize, end: isize);
    fn onattribdata(&mut self, start: isize, end: isize);
    fn onattribentity(&mut self, cp: u32);
    fn onattribend(&mut self, quote: QuoteType, end: isize);
    fn oncomment(&mut self, start: isize, end: isize, offset: isize);
    fn oncdata(&mut self, start: isize, end: isize, offset: isize);
    fn ondeclaration(&mut self, start: isize, end: isize);
    fn onprocessinginstruction(&mut self, start: isize, end: isize);
    fn onend(&mut self);
}

pub(super) struct Tokenizer {
    state: State,
    /// End (exclusive, global index) of the data written so far.
    end: isize,
    section_start: isize,
    index: isize,
    base_state: State,
    is_special: bool,
    running: bool,
    current_sequence: Sequence,
    sequence_index: usize,
    trie_index: i64,
    trie_current: u16,
    entity_result: u64,
    entity_excess: isize,
}

impl Tokenizer {
    pub(super) fn new() -> Self {
        Tokenizer {
            state: State::Text,
            end: 0,
            section_start: 0,
            index: 0,
            base_state: State::Text,
            is_special: false,
            running: true,
            current_sequence: Sequence::Cdata,
            sequence_index: 0,
            trie_index: 0,
            trie_current: 0,
            entity_result: 0,
            entity_excess: 0,
        }
    }

    /// Make the input available up to `end` (global, exclusive) and parse it.
    pub(super) fn write<S: TokenSink>(&mut self, end: isize, sink: &mut S) {
        self.end = end;
        self.parse(sink);
    }

    pub(super) fn end<S: TokenSink>(&mut self, sink: &mut S) {
        if self.running {
            self.finish(sink);
        }
    }

    fn char_at<S: TokenSink>(&self, sink: &S, index: isize) -> u8 {
        sink.input()[index as usize]
    }

    fn state_text<S: TokenSink>(&mut self, c: u8, sink: &mut S) {
        if c == LT {
            if self.index > self.section_start {
                sink.ontext(self.section_start, self.index);
            }
            self.state = State::BeforeTagName;
            self.section_start = self.index;
        } else if c == AMP {
            self.state = State::BeforeEntity;
        }
    }

    fn state_special_start_sequence<S: TokenSink>(&mut self, c: u8, sink: &mut S) {
        let seq = self.current_sequence.bytes();
        let is_end = self.sequence_index == seq.len();
        let is_match = if is_end { is_end_of_tag_section(c) } else { (c | 0x20) == seq[self.sequence_index] };
        if !is_match {
            self.is_special = false;
        } else if !is_end {
            self.sequence_index += 1;
            return;
        }
        self.sequence_index = 0;
        self.state = State::InTagName;
        self.state_in_tag_name(c, sink);
    }

    fn state_in_special_tag<S: TokenSink>(&mut self, c: u8, sink: &mut S) {
        let seq = self.current_sequence.bytes();
        if self.sequence_index == seq.len() {
            if c == GT || is_whitespace(c) {
                let end_of_text = self.index - seq.len() as isize;
                if self.section_start < end_of_text {
                    let actual_index = self.index;
                    self.index = end_of_text;
                    sink.ontext(self.section_start, end_of_text);
                    self.index = actual_index;
                }
                self.is_special = false;
                self.section_start = end_of_text + 2;
                self.state_in_closing_tag_name(c, sink);
                return;
            }
            self.sequence_index = 0;
        }
        if (c | 0x20) == seq[self.sequence_index] {
            self.sequence_index += 1;
        } else if self.sequence_index == 0 {
            if self.current_sequence == Sequence::TitleEnd {
                if c == AMP {
                    self.state = State::BeforeEntity;
                }
            } else if self.fast_forward_to(LT, sink) {
                self.sequence_index = 1;
            }
        } else {
            self.sequence_index = usize::from(c == LT);
        }
    }

    fn state_cdata_sequence<S: TokenSink>(&mut self, c: u8, sink: &mut S) {
        let seq = Sequence::Cdata.bytes();
        if c == seq[self.sequence_index] {
            self.sequence_index += 1;
            if self.sequence_index == seq.len() {
                self.state = State::InCommentLike;
                self.current_sequence = Sequence::CdataEnd;
                self.sequence_index = 0;
                self.section_start = self.index + 1;
            }
        } else {
            self.sequence_index = 0;
            self.state = State::InDeclaration;
            self.state_in_declaration(c, sink);
        }
    }

    fn fast_forward_to<S: TokenSink>(&mut self, c: u8, sink: &S) -> bool {
        loop {
            self.index += 1;
            if self.index >= self.end {
                break;
            }
            if self.char_at(sink, self.index) == c {
                return true;
            }
        }
        self.index = self.end - 1;
        false
    }

    fn state_in_comment_like<S: TokenSink>(&mut self, c: u8, sink: &mut S) {
        let seq = self.current_sequence.bytes();
        if c == seq[self.sequence_index] {
            self.sequence_index += 1;
            if self.sequence_index == seq.len() {
                if self.current_sequence == Sequence::CdataEnd {
                    sink.oncdata(self.section_start, self.index, 2);
                } else {
                    sink.oncomment(self.section_start, self.index, 2);
                }
                self.sequence_index = 0;
                self.section_start = self.index + 1;
                self.state = State::Text;
            }
        } else if self.sequence_index == 0 {
            if self.fast_forward_to(seq[0], sink) {
                self.sequence_index = 1;
            }
        } else if c != seq[self.sequence_index - 1] {
            self.sequence_index = 0;
        }
    }

    fn start_special(&mut self, sequence: Sequence, offset: usize) {
        self.is_special = true;
        self.current_sequence = sequence;
        self.sequence_index = offset;
        self.state = State::SpecialStartSequence;
    }

    fn state_before_tag_name<S: TokenSink>(&mut self, c: u8, sink: &mut S) {
        if c == EXCLAMATION_MARK {
            self.state = State::BeforeDeclaration;
            self.section_start = self.index + 1;
        } else if c == QUESTIONMARK {
            self.state = State::InProcessingInstruction;
            self.section_start = self.index + 1;
        } else if is_ascii_alpha(c) {
            let lower = c | 0x20;
            self.section_start = self.index;
            if lower == Sequence::TitleEnd.bytes()[2] {
                self.start_special(Sequence::TitleEnd, 3);
            } else {
                self.state =
                    if lower == Sequence::ScriptEnd.bytes()[2] { State::BeforeSpecialS } else { State::InTagName };
            }
        } else if c == SLASH {
            self.state = State::BeforeClosingTagName;
        } else {
            self.state = State::Text;
            self.state_text(c, sink);
        }
    }

    fn state_in_tag_name<S: TokenSink>(&mut self, c: u8, sink: &mut S) {
        if is_end_of_tag_section(c) {
            sink.onopentagname(self.section_start, self.index);
            self.section_start = -1;
            self.state = State::BeforeAttributeName;
            self.state_before_attribute_name(c, sink);
        }
    }

    fn state_before_closing_tag_name(&mut self, c: u8) {
        if is_whitespace(c) {
            // Ignore
        } else if c == GT {
            self.state = State::Text;
        } else {
            self.state = if is_ascii_alpha(c) { State::InClosingTagName } else { State::InSpecialComment };
            self.section_start = self.index;
        }
    }

    fn state_in_closing_tag_name<S: TokenSink>(&mut self, c: u8, sink: &mut S) {
        if c == GT || is_whitespace(c) {
            sink.onclosetag(self.section_start, self.index);
            self.section_start = -1;
            self.state = State::AfterClosingTagName;
            self.state_after_closing_tag_name(c, sink);
        }
    }

    fn state_after_closing_tag_name<S: TokenSink>(&mut self, c: u8, sink: &mut S) {
        if c == GT || self.fast_forward_to(GT, sink) {
            self.state = State::Text;
            self.base_state = State::Text;
            self.section_start = self.index + 1;
        }
    }

    fn state_before_attribute_name<S: TokenSink>(&mut self, c: u8, sink: &mut S) {
        if c == GT {
            sink.onopentagend(self.index);
            if self.is_special {
                self.state = State::InSpecialTag;
                self.sequence_index = 0;
            } else {
                self.state = State::Text;
            }
            self.base_state = self.state;
            self.section_start = self.index + 1;
        } else if c == SLASH {
            self.state = State::InSelfClosingTag;
        } else if !is_whitespace(c) {
            self.state = State::InAttributeName;
            self.section_start = self.index;
        }
    }

    fn state_in_self_closing_tag<S: TokenSink>(&mut self, c: u8, sink: &mut S) {
        if c == GT {
            sink.onselfclosingtag(self.index);
            self.state = State::Text;
            self.base_state = State::Text;
            self.section_start = self.index + 1;
            self.is_special = false;
        } else if !is_whitespace(c) {
            self.state = State::BeforeAttributeName;
            self.state_before_attribute_name(c, sink);
        }
    }

    fn state_in_attribute_name<S: TokenSink>(&mut self, c: u8, sink: &mut S) {
        if c == EQ || is_end_of_tag_section(c) {
            sink.onattribname(self.section_start, self.index);
            self.section_start = -1;
            self.state = State::AfterAttributeName;
            self.state_after_attribute_name(c, sink);
        }
    }

    fn state_after_attribute_name<S: TokenSink>(&mut self, c: u8, sink: &mut S) {
        if c == EQ {
            self.state = State::BeforeAttributeValue;
        } else if c == SLASH || c == GT {
            sink.onattribend(QuoteType::NoValue, self.index);
            self.state = State::BeforeAttributeName;
            self.state_before_attribute_name(c, sink);
        } else if !is_whitespace(c) {
            sink.onattribend(QuoteType::NoValue, self.index);
            self.state = State::InAttributeName;
            self.section_start = self.index;
        }
    }

    fn state_before_attribute_value<S: TokenSink>(&mut self, c: u8, sink: &mut S) {
        if c == DOUBLE_QUOTE {
            self.state = State::InAttributeValueDq;
            self.section_start = self.index + 1;
        } else if c == SINGLE_QUOTE {
            self.state = State::InAttributeValueSq;
            self.section_start = self.index + 1;
        } else if !is_whitespace(c) {
            self.section_start = self.index;
            self.state = State::InAttributeValueNq;
            self.state_in_attribute_value_no_quotes(c, sink);
        }
    }

    fn handle_in_attribute_value<S: TokenSink>(&mut self, c: u8, quote: u8, sink: &mut S) {
        if c == quote {
            sink.onattribdata(self.section_start, self.index);
            self.section_start = -1;
            sink.onattribend(if quote == DOUBLE_QUOTE { QuoteType::Double } else { QuoteType::Single }, self.index);
            self.state = State::BeforeAttributeName;
        } else if c == AMP {
            self.base_state = self.state;
            self.state = State::BeforeEntity;
        }
    }

    fn state_in_attribute_value_no_quotes<S: TokenSink>(&mut self, c: u8, sink: &mut S) {
        if is_whitespace(c) || c == GT {
            sink.onattribdata(self.section_start, self.index);
            self.section_start = -1;
            sink.onattribend(QuoteType::Unquoted, self.index);
            self.state = State::BeforeAttributeName;
            self.state_before_attribute_name(c, sink);
        } else if c == AMP {
            self.base_state = self.state;
            self.state = State::BeforeEntity;
        }
    }

    fn state_before_declaration(&mut self, c: u8) {
        if c == OPENING_SQUARE_BRACKET {
            self.state = State::CdataSequence;
            self.sequence_index = 0;
        } else {
            self.state = if c == DASH { State::BeforeComment } else { State::InDeclaration };
        }
    }

    fn state_in_declaration<S: TokenSink>(&mut self, c: u8, sink: &mut S) {
        if c == GT || self.fast_forward_to(GT, sink) {
            sink.ondeclaration(self.section_start, self.index);
            self.state = State::Text;
            self.section_start = self.index + 1;
        }
    }

    fn state_in_processing_instruction<S: TokenSink>(&mut self, c: u8, sink: &mut S) {
        if c == GT || self.fast_forward_to(GT, sink) {
            sink.onprocessinginstruction(self.section_start, self.index);
            self.state = State::Text;
            self.section_start = self.index + 1;
        }
    }

    fn state_before_comment(&mut self, c: u8) {
        if c == DASH {
            self.state = State::InCommentLike;
            self.current_sequence = Sequence::CommentEnd;
            self.sequence_index = 2;
            self.section_start = self.index + 1;
        } else {
            self.state = State::InDeclaration;
        }
    }

    fn state_in_special_comment<S: TokenSink>(&mut self, c: u8, sink: &mut S) {
        if c == GT || self.fast_forward_to(GT, sink) {
            sink.oncomment(self.section_start, self.index, 0);
            self.state = State::Text;
            self.section_start = self.index + 1;
        }
    }

    fn state_before_special_s<S: TokenSink>(&mut self, c: u8, sink: &mut S) {
        let lower = c | 0x20;
        if lower == Sequence::ScriptEnd.bytes()[3] {
            self.start_special(Sequence::ScriptEnd, 4);
        } else if lower == Sequence::StyleEnd.bytes()[3] {
            self.start_special(Sequence::StyleEnd, 4);
        } else {
            self.state = State::InTagName;
            self.state_in_tag_name(c, sink);
        }
    }

    fn state_before_entity<S: TokenSink>(&mut self, c: u8, sink: &mut S) {
        self.entity_excess = 1;
        self.entity_result = 0;
        if c == NUMBER {
            self.state = State::BeforeNumericEntity;
        } else if c == AMP {
            // Two `&` in a row: stay in the current state.
        } else {
            self.trie_index = 0;
            self.trie_current = HTML_DECODE_TREE[0];
            self.state = State::InNamedEntity;
            self.state_in_named_entity(c, sink);
        }
    }

    fn state_in_named_entity<S: TokenSink>(&mut self, c: u8, sink: &mut S) {
        self.entity_excess += 1;
        self.trie_index = determine_branch(&HTML_DECODE_TREE, self.trie_current, self.trie_index + 1, c as u16);
        if self.trie_index < 0 {
            self.emit_named_entity(sink);
            self.index -= 1;
            return;
        }
        self.trie_current = tree_at(&HTML_DECODE_TREE, self.trie_index);
        let masked = self.trie_current & VALUE_LENGTH;
        if masked != 0 {
            let value_length = ((masked >> 14) as i64) - 1;
            if !self.allow_legacy_entity() && c != SEMI {
                self.trie_index += value_length;
            } else {
                let entity_start = self.index - self.entity_excess + 1;
                if entity_start > self.section_start {
                    self.emit_partial(self.section_start, entity_start, sink);
                }
                self.entity_result = self.trie_index as u64;
                self.trie_index += value_length;
                self.entity_excess = 0;
                self.section_start = self.index + 1;
                if value_length == 0 {
                    self.emit_named_entity(sink);
                }
            }
        }
    }

    fn emit_named_entity<S: TokenSink>(&mut self, sink: &mut S) {
        self.state = self.base_state;
        if self.entity_result == 0 {
            return;
        }
        let r = self.entity_result as i64;
        let value_length = (tree_at(&HTML_DECODE_TREE, r) & VALUE_LENGTH) >> 14;
        match value_length {
            1 => self.emit_code_point((tree_at(&HTML_DECODE_TREE, r) & !VALUE_LENGTH) as u32, sink),
            2 => self.emit_code_point(tree_at(&HTML_DECODE_TREE, r + 1) as u32, sink),
            3 => {
                self.emit_code_point(tree_at(&HTML_DECODE_TREE, r + 1) as u32, sink);
                self.emit_code_point(tree_at(&HTML_DECODE_TREE, r + 2) as u32, sink);
            }
            _ => {}
        }
    }

    fn state_before_numeric_entity<S: TokenSink>(&mut self, c: u8, sink: &mut S) {
        if (c | 0x20) == LOWER_X {
            self.entity_excess += 1;
            self.state = State::InHexEntity;
        } else {
            self.state = State::InNumericEntity;
            self.state_in_numeric_entity(c, sink);
        }
    }

    fn emit_numeric_entity<S: TokenSink>(&mut self, strict: bool, sink: &mut S) {
        let entity_start = self.index - self.entity_excess - 1;
        let number_start = entity_start + 2 + isize::from(self.state == State::InHexEntity);
        if number_start != self.index {
            if entity_start > self.section_start {
                self.emit_partial(self.section_start, entity_start, sink);
            }
            self.section_start = self.index + isize::from(strict);
            self.emit_code_point(replace_code_point(self.entity_result), sink);
        }
        self.state = self.base_state;
    }

    fn state_in_numeric_entity<S: TokenSink>(&mut self, c: u8, sink: &mut S) {
        if c == SEMI {
            self.emit_numeric_entity(true, sink);
        } else if is_number(c) {
            self.entity_result = self.entity_result.saturating_mul(10).saturating_add((c - ZERO) as u64);
            self.entity_excess += 1;
        } else {
            if self.allow_legacy_entity() {
                self.emit_numeric_entity(false, sink);
            } else {
                self.state = self.base_state;
            }
            self.index -= 1;
        }
    }

    fn state_in_hex_entity<S: TokenSink>(&mut self, c: u8, sink: &mut S) {
        if c == SEMI {
            self.emit_numeric_entity(true, sink);
        } else if is_number(c) {
            self.entity_result = self.entity_result.saturating_mul(16).saturating_add((c - ZERO) as u64);
            self.entity_excess += 1;
        } else if is_hex_digit(c) {
            self.entity_result =
                self.entity_result.saturating_mul(16).saturating_add(((c | 0x20) - LOWER_A + 10) as u64);
            self.entity_excess += 1;
        } else {
            if self.allow_legacy_entity() {
                self.emit_numeric_entity(false, sink);
            } else {
                self.state = self.base_state;
            }
            self.index -= 1;
        }
    }

    fn allow_legacy_entity(&self) -> bool {
        self.base_state == State::Text || self.base_state == State::InSpecialTag
    }

    fn cleanup<S: TokenSink>(&mut self, sink: &mut S) {
        if self.running && self.section_start != self.index {
            if self.state == State::Text || (self.state == State::InSpecialTag && self.sequence_index == 0) {
                sink.ontext(self.section_start, self.index);
                self.section_start = self.index;
            } else if self.state == State::InAttributeValueDq
                || self.state == State::InAttributeValueSq
                || self.state == State::InAttributeValueNq
            {
                sink.onattribdata(self.section_start, self.index);
                self.section_start = self.index;
            }
        }
    }

    fn should_continue(&self) -> bool {
        self.index < self.end && self.running
    }

    fn parse<S: TokenSink>(&mut self, sink: &mut S) {
        while self.should_continue() {
            if self.state == State::Text {
                // `stateText` only reacts to `<` and `&`; skip everything else in bulk.
                let start = self.index as usize;
                let end = self.end as usize;
                match sink.input()[start..end].iter().position(|&c| c == LT || c == AMP) {
                    Some(offset) => self.index = (start + offset) as isize,
                    None => {
                        self.index = self.end;
                        break;
                    }
                }
            }
            let c = self.char_at(sink, self.index);
            match self.state {
                State::Text => self.state_text(c, sink),
                State::SpecialStartSequence => self.state_special_start_sequence(c, sink),
                State::InSpecialTag => self.state_in_special_tag(c, sink),
                State::CdataSequence => self.state_cdata_sequence(c, sink),
                State::InAttributeValueDq => self.handle_in_attribute_value(c, DOUBLE_QUOTE, sink),
                State::InAttributeName => self.state_in_attribute_name(c, sink),
                State::InCommentLike => self.state_in_comment_like(c, sink),
                State::InSpecialComment => self.state_in_special_comment(c, sink),
                State::BeforeAttributeName => self.state_before_attribute_name(c, sink),
                State::InTagName => self.state_in_tag_name(c, sink),
                State::InClosingTagName => self.state_in_closing_tag_name(c, sink),
                State::BeforeTagName => self.state_before_tag_name(c, sink),
                State::AfterAttributeName => self.state_after_attribute_name(c, sink),
                State::InAttributeValueSq => self.handle_in_attribute_value(c, SINGLE_QUOTE, sink),
                State::BeforeAttributeValue => self.state_before_attribute_value(c, sink),
                State::BeforeClosingTagName => self.state_before_closing_tag_name(c),
                State::AfterClosingTagName => self.state_after_closing_tag_name(c, sink),
                State::BeforeSpecialS => self.state_before_special_s(c, sink),
                State::InAttributeValueNq => self.state_in_attribute_value_no_quotes(c, sink),
                State::InSelfClosingTag => self.state_in_self_closing_tag(c, sink),
                State::InDeclaration => self.state_in_declaration(c, sink),
                State::BeforeDeclaration => self.state_before_declaration(c),
                State::BeforeComment => self.state_before_comment(c),
                State::InProcessingInstruction => self.state_in_processing_instruction(c, sink),
                State::InNamedEntity => self.state_in_named_entity(c, sink),
                State::BeforeEntity => self.state_before_entity(c, sink),
                State::InHexEntity => self.state_in_hex_entity(c, sink),
                State::InNumericEntity => self.state_in_numeric_entity(c, sink),
                State::BeforeNumericEntity => self.state_before_numeric_entity(c, sink),
            }
            self.index += 1;
        }
        self.cleanup(sink);
    }

    fn finish<S: TokenSink>(&mut self, sink: &mut S) {
        if self.state == State::InNamedEntity {
            self.emit_named_entity(sink);
        }
        if self.section_start < self.index {
            self.handle_trailing_data(sink);
        }
        sink.onend();
    }

    fn handle_trailing_data<S: TokenSink>(&mut self, sink: &mut S) {
        let end_index = self.end;
        if self.state == State::InCommentLike {
            if self.current_sequence == Sequence::CdataEnd {
                sink.oncdata(self.section_start, end_index, 0);
            } else {
                sink.oncomment(self.section_start, end_index, 0);
            }
        } else if (self.state == State::InNumericEntity || self.state == State::InHexEntity)
            && self.allow_legacy_entity()
        {
            self.emit_numeric_entity(false, sink);
        } else if matches!(
            self.state,
            State::InTagName
                | State::BeforeAttributeName
                | State::BeforeAttributeValue
                | State::AfterAttributeName
                | State::InAttributeName
                | State::InAttributeValueSq
                | State::InAttributeValueDq
                | State::InAttributeValueNq
                | State::InClosingTagName
        ) {
            // Signals that the tag should be ignored.
        } else {
            sink.ontext(self.section_start, end_index);
        }
    }

    fn emit_partial<S: TokenSink>(&mut self, start: isize, end_index: isize, sink: &mut S) {
        if self.base_state != State::Text && self.base_state != State::InSpecialTag {
            sink.onattribdata(start, end_index);
        } else {
            sink.ontext(start, end_index);
        }
    }

    fn emit_code_point<S: TokenSink>(&mut self, cp: u32, sink: &mut S) {
        if self.base_state != State::Text && self.base_state != State::InSpecialTag {
            sink.onattribentity(cp);
        } else {
            sink.ontextentity(cp);
        }
    }
}
