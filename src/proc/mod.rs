use core::fmt;
use std::fmt::{Debug, Formatter};
use std::ops::{Index, IndexMut};

use aho_corasick::AhoCorasick;

use crate::err::{Error, ErrorType, ProcessingResult, debug_repr};
use crate::proc::MatchAction::*;
use crate::proc::MatchMode::*;
use crate::proc::range::ProcessorRange;
use memchr::memchr;
use crate::gen::codepoints::Lookup;

#[cfg(feature = "js-esbuild")]
use {
    std::sync::{Arc, Mutex},
    crossbeam::sync::WaitGroup,
    esbuild_rs::TransformResult,
};

pub mod checkpoint;
pub mod entity;
pub mod range;

#[allow(dead_code)]
pub enum MatchMode {
    IsChar(u8),
    IsNotChar(u8),
    WhileChar(u8),
    WhileNotChar(u8),
    // Through is like WhileNot followed by Is, but matches zero if Is is zero.
    ThroughChar(u8),

    IsPred(fn(u8) -> bool),
    IsNotPred(fn(u8) -> bool),
    WhilePred(fn(u8) -> bool),
    WhileNotPred(fn(u8) -> bool),

    IsInLookup(&'static Lookup),
    WhileInLookup(&'static Lookup),
    WhileNotInLookup(&'static Lookup),

    IsSeq(&'static [u8]),
    WhileNotSeq(&'static AhoCorasick),
    ThroughSeq(&'static AhoCorasick),
}

pub enum MatchAction {
    Keep,
    Discard,
    MatchOnly,
}

#[cfg(feature = "js-esbuild")]
pub struct JsMinSection {
    pub src: ProcessorRange,
    pub result: TransformResult,
}

// Processing state of a file. Single use only; create one per processing.
pub struct Processor<'d> {
    code: &'d mut [u8],
    // Index of the next character to read.
    read_next: usize,
    // Index of the next unwritten space.
    write_next: usize,
    #[cfg(feature = "js-esbuild")]
    script_wg: WaitGroup,
    #[cfg(feature = "js-esbuild")]
    script_results: Arc<Mutex<Vec<JsMinSection>>>,
}

impl<'d> Index<ProcessorRange> for Processor<'d> {
    type Output = [u8];

    #[inline(always)]
    fn index(&self, index: ProcessorRange) -> &Self::Output {
        &self.code[index.start..index.end]
    }
}

impl<'d> IndexMut<ProcessorRange> for Processor<'d> {
    #[inline(always)]
    fn index_mut(&mut self, index: ProcessorRange) -> &mut Self::Output {
        debug_assert!(index.end <= self.write_next);
        &mut self.code[index.start..index.end]
    }
}

#[allow(dead_code)]
impl<'d> Processor<'d> {
    // Constructor.
    #[inline(always)]
    pub fn new(code: &mut [u8]) -> Processor {
        Processor {
            write_next: 0,
            read_next: 0,
            code,
            #[cfg(feature = "js-esbuild")]
            script_wg: WaitGroup::new(),
            #[cfg(feature = "js-esbuild")]
            script_results: Arc::new(Mutex::new(Vec::new())),
        }
    }

    // INTERNAL APIs.
    // Bounds checking.
    #[inline(always)]
    fn _in_bounds(&self, offset: usize) -> bool {
        self.read_next + offset < self.code.len()
    }

    // Reading.
    /// Get the `offset` character from next.
    /// When `offset` is 0, the next character is returned.
    /// Panics. Does not check bounds for performance (e.g. already checked).
    #[inline(always)]
    fn _read_offset(&self, offset: usize) -> u8 {
        self.code[self.read_next + offset]
    }

    #[inline(always)]
    fn _maybe_read_offset(&self, offset: usize) -> Option<u8> {
        self.code.get(self.read_next + offset).map(|c| *c)
    }

    #[inline(always)]
    fn _maybe_read_slice_offset(&self, offset: usize, count: usize) -> Option<&[u8]> {
        self.code.get(self.read_next + offset..self.read_next + offset + count)
    }

    /// Move next `amount` characters to output.
    /// Panics. Does not check bounds for performance (e.g. already checked).
    #[inline(always)]
    fn _shift(&mut self, amount: usize) -> () {
        // Optimisation: Don't shift if already there (but still update offsets).
        if self.read_next != self.write_next {
            self.code.copy_within(self.read_next..self.read_next + amount, self.write_next);
        };
        self.read_next += amount;
        self.write_next += amount;
    }

    #[inline(always)]
    fn _replace(&mut self, start: usize, end: usize, data: &[u8]) -> usize {
        debug_assert!(start <= end);
        let added = data.len() - (end - start);
        // Do not allow writing over source.
        debug_assert!(self.write_next + added <= self.read_next);
        self.code.copy_within(end..self.write_next, end + added);
        self.code[start..start + data.len()].copy_from_slice(data);
        // Don't need to update read_next as only data before it has changed.
        self.write_next += added;
        added
    }

    #[inline(always)]
    fn _insert(&mut self, at: usize, data: &[u8]) -> usize {
        self._replace(at, at, data)
    }

    // Matching.
    #[inline(always)]
    fn _one<C: FnOnce(u8) -> bool>(&mut self, cond: C) -> usize {
        self._maybe_read_offset(0).filter(|n| cond(*n)).is_some() as usize
    }

    #[inline(always)]
    fn _many<C: Fn(u8) -> bool>(&mut self, cond: C) -> usize {
        let mut count = 0usize;
        while self._maybe_read_offset(count).filter(|c| cond(*c)).is_some() {
            count += 1;
        };
        count
    }

    #[inline(always)]
    fn _remaining(&self) -> usize {
        self.code.len() - self.read_next
    }

    #[inline(always)]
    pub fn m(&mut self, mode: MatchMode, action: MatchAction) -> ProcessorRange {
        let count = match mode {
            IsChar(c) => self._one(|n| n == c),
            IsNotChar(c) => self._one(|n| n != c),
            WhileChar(c) => self._many(|n| n == c),
            WhileNotChar(c) => memchr(c, &self.code[self.read_next..]).unwrap_or(self._remaining()),
            ThroughChar(c) => memchr(c, &self.code[self.read_next..]).map_or(0, |p| p + 1),

            IsInLookup(lookup) => self._one(|n| lookup[n]),
            WhileInLookup(lookup) => self._many(|n| lookup[n]),
            WhileNotInLookup(lookup) => self._many(|n| !lookup[n]),

            IsPred(p) => self._one(|n| p(n)),
            IsNotPred(p) => self._one(|n| !p(n)),
            WhilePred(p) => self._many(|n| p(n)),
            WhileNotPred(p) => self._many(|n| !p(n)),

            IsSeq(seq) => self._maybe_read_slice_offset(0, seq.len()).filter(|src| *src == seq).map_or(0, |_| seq.len()),
            WhileNotSeq(seq) => seq.find(&self.code[self.read_next..]).map_or(self._remaining(), |m| m.start()),
            // Match.end is exclusive, so do not add one.
            ThroughSeq(seq) => seq.find(&self.code[self.read_next..]).map_or(0, |m| m.end()),
        };
        // If keeping, match will be available in written range (which is better as source might eventually get overwritten).
        // If discarding, then only option is source range.
        let start = match action {
            Discard | MatchOnly => self.read_next,
            Keep => self.write_next,
        };
        match action {
            Discard => self.read_next += count,
            Keep => self._shift(count),
            MatchOnly => {}
        };

        ProcessorRange { start, end: start + count }
    }

    // PUBLIC APIs.
    // Bounds checking
    #[inline(always)]
    pub fn at_end(&self) -> bool {
        !self._in_bounds(0)
    }

    #[inline(always)]
    pub fn require_not_at_end(&self) -> ProcessingResult<()> {
        if self.at_end() {
            Err(ErrorType::UnexpectedEnd)
        } else {
            Ok(())
        }
    }

    /// Get how many characters have been consumed from source.
    #[inline(always)]
    pub fn read_len(&self) -> usize {
        self.read_next
    }

    #[inline(always)]
    pub fn reserve_output(&mut self, amount: usize) -> () {
        self.write_next += amount;
    }

    // Looking ahead.
    /// Get the `offset` character from next.
    /// When `offset` is 0, the next character is returned.
    #[inline(always)]
    pub fn peek(&self, offset: usize) -> Option<u8> {
        self._maybe_read_offset(offset)
    }

    #[inline(always)]
    pub fn peek_many(&self, offset: usize, count: usize) -> Option<&[u8]> {
        self._maybe_read_slice_offset(offset, count)
    }

    // Looking behind.
    #[inline(always)]
    pub fn last(&self, count: usize) -> Option<&[u8]> {
        if count > self.write_next {
            None
        } else {
            self.code.get(self.write_next - count..self.write_next)
        }
    }

    // Consuming source characters.
    /// Skip and return the next character.
    /// Will result in an error if exceeds bounds.
    #[inline(always)]
    pub fn skip(&mut self) -> ProcessingResult<u8> {
        self._maybe_read_offset(0).map(|c| {
            self.read_next += 1;
            c
        }).ok_or(ErrorType::UnexpectedEnd)
    }

    #[inline(always)]
    pub fn skip_amount_expect(&mut self, amount: usize) -> () {
        debug_assert!(!self.at_end(), "skip known characters");
        self.read_next += amount;
    }

    #[inline(always)]
    pub fn undo_skip_expect(&mut self) -> () {
        debug_assert!(!self.at_end(), "revert skip known character");
        self.read_next -= 1;
    }

    #[inline(always)]
    pub fn skip_expect(&mut self) -> () {
        debug_assert!(!self.at_end(), "skip known character");
        self.read_next += 1;
    }

    // Writing characters directly.
    /// Write `c` to output. Will panic if exceeds bounds.
    #[inline(always)]
    pub fn write(&mut self, c: u8) -> () {
        self.code[self.write_next] = c;
        self.write_next += 1;
    }

    #[inline(always)]
    pub fn make_lowercase(&mut self, range: ProcessorRange) -> () {
        self.code[range.start..range.end].make_ascii_lowercase();
    }

    #[inline(always)]
    pub fn write_range(&mut self, s: ProcessorRange) -> ProcessorRange {
        let dest_start = self.write_next;
        let dest_end = dest_start + s.len();
        self.code.copy_within(s.start..s.end, dest_start);
        self.write_next = dest_end;
        ProcessorRange { start: dest_start, end: dest_end }
    }

    /// Write `s` to output. Will panic if exceeds bounds.
    #[inline(always)]
    pub fn write_slice(&mut self, s: &[u8]) -> () {
        self.code[self.write_next..self.write_next + s.len()].copy_from_slice(s);
        self.write_next += s.len();
    }

    #[inline(always)]
    pub fn write_utf8(&mut self, c: char) -> () {
        let mut encoded = [0u8; 4];
        self.write_slice(c.encode_utf8(&mut encoded).as_bytes());
    }

    // Shifting characters.
    #[inline(always)]
    pub fn accept(&mut self) -> ProcessingResult<u8> {
        self._maybe_read_offset(0).map(|c| {
            self.code[self.write_next] = c;
            self.read_next += 1;
            self.write_next += 1;
            c
        }).ok_or(ErrorType::UnexpectedEnd)
    }

    #[inline(always)]
    pub fn accept_expect(&mut self) -> u8 {
        debug_assert!(!self.at_end());
        let c = self._read_offset(0);
        self.code[self.write_next] = c;
        self.read_next += 1;
        self.write_next += 1;
        c
    }

    #[inline(always)]
    pub fn accept_amount_expect(&mut self, count: usize) -> () {
        debug_assert!(self._in_bounds(count - 1));
        self._shift(count);
    }

    #[cfg(feature = "js-esbuild")]
    #[inline(always)]
    pub fn new_script_section(&self) -> (WaitGroup, Arc<Mutex<Vec<JsMinSection>>>) {
        (self.script_wg.clone(), self.script_results.clone())
    }

    // Since we consume the Processor, we must provide a full Error with positions.
    #[cfg(not(feature = "js-esbuild"))]
    #[inline(always)]
    pub fn finish(self) -> Result<usize, Error> {
        // NOTE: Do not assert that we are at the end, as invalid HTML can end prematurely e.g.
        // `<html>hello</html>outside`.
        Ok(self.write_next)
    }

    // Since we consume the Processor, we must provide a full Error with positions.
    #[cfg(feature = "js-esbuild")]
    #[inline(always)]
    pub fn finish(self) -> Result<usize, Error> {
        // NOTE: Do not assert that we are at the end, as invalid HTML can end prematurely e.g.
        // `<html>hello</html>outside`.
        self.script_wg.wait();
        let mut results = Arc::try_unwrap(self.script_results)
            .unwrap_or_else(|_| panic!("failed to acquire script results"))
            .into_inner()
            .unwrap();
        results.sort_unstable_by_key(|r| r.src.start);
        // As we write minified JS code for sections from left to right, we will be shifting code
        // towards the left as previous source JS code sections shrink. We need to keep track of
        // the write pointer after previous compaction.
        // If there are no script sections, then we get self.write_next which will be returned.
        let mut write_next = results.get(0).map_or(self.write_next, |r| r.src.start);
        for (i, JsMinSection { result, src }) in results.iter().enumerate() {
            // Resulting minified JS to write.
            // TODO Verify.
            // TODO Rewrite these in esbuild fork so we don't have to do a memcpy and search+replace.
            let min_js = result.js.as_str().trim().replace("</script", "<\\/script");
            let js_len = if min_js.len() < src.len() {
                self.code[write_next..write_next + min_js.len()].copy_from_slice(min_js.as_bytes());
                min_js.len()
            } else {
                // If minified result is actually longer than source, then write source instead.
                // NOTE: We still need to write source as previous iterations may have shifted code down.
                self.code.copy_within(src.start..src.end, write_next);
                src.len()
            };
            let write_end = write_next + js_len;
            let next_start = results.get(i + 1).map_or(self.write_next, |r| r.src.start);
            self.code.copy_within(src.end..next_start, write_end);
            write_next = write_end + (next_start - src.end);
        };
        Ok(write_next)
    }
}

impl Debug for Processor<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(&debug_repr(self.code, self.read_next as isize, self.write_next as isize))?;
        Ok(())
    }
}
