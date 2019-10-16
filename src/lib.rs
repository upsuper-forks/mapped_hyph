/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

#[macro_use]
extern crate arrayref;
extern crate memmap;

use std::slice;
use std::str;

use std::cmp::max;
use std::ffi::CStr;
use std::fs::File;
use std::ops::Deref;
use std::os::raw::c_char;

use memmap::Mmap;

const INVALID_STRING_OFFSET: usize = 0xffff;
const INVALID_STATE_OFFSET: usize = 0xffffff;

// Transition actually holds a 24-bit new state offset and an 8-bit input byte
// to match. We will be interpreting byte ranges as Transition arrays (in the
// State::transitions() method below), so use repr(C) to ensure we have the
// memory layout we expect.
#[repr(C)]
#[derive(Debug,Clone,Copy)]
struct Transition ( u8, u8, u8, u8 );

impl Transition {
    fn new_state_offset(&self) -> usize {
        self.0 as usize + self.1 as usize * 0x100 + self.2 as usize * 0x10000
    }
    fn match_byte(&self) -> u8 {
        self.3
    }
}

// State is a reference to a slice of mmap'd data that begins with a fixed
// header, followed by an array of transitions. Total size of the data slice
// depends on the number of transitions in the state.
// There are two versions of State, a basic record that supports only simple
// hyphenation (no associated spelling change), and an extended version that
// adds the replacement-string fields to support spelling changes at the
// hyphenation point. Check is_extended() to know which version is present.
#[derive(Debug,Copy,Clone)]
struct State<'a> {
    data: &'a [u8],
}

impl State<'_> {
    // Accessors for the various State header fields.
    fn fallback_state(&self) -> usize {
        u32::from_le_bytes(*array_ref!(self.data, 0, 4)) as usize
    }
    fn match_string_offset(&self) -> usize {
        u16::from_le_bytes(*array_ref!(self.data, 4, 2)) as usize
    }
    fn num_transitions(&self) -> u8 {
        self.data[6]
    }
    fn is_extended(&self) -> bool {
        self.data[7] != 0
    }
    // Accessors that are only valid if is_extended() is true.
    fn repl_string_offset(&self) -> usize {
        u16::from_le_bytes(*array_ref!(self.data, 8, 2)) as usize
    }
    fn repl_index(&self) -> i8 {
        self.data[10] as i8
    }
    fn repl_cut(&self) -> i8 {
        self.data[11] as i8
    }
    // Return the state's Transitions as a slice reference.
    fn transitions(&self) -> &[Transition] {
        let count = self.num_transitions() as usize;
        if count == 0 {
            return &[];
        }
        let transition_offset = if self.is_extended() { 12 } else { 8 };
        assert!(self.data.len() == transition_offset + count * 4);
        let trans_ptr = &self.data[transition_offset] as *const u8 as *const Transition;
        unsafe { slice::from_raw_parts(trans_ptr, count) }
    }
    // Look up the Transition for a given input byte, or None.
    fn transition_for(&self, b: u8) -> Option<Transition> {
        // TODO: investigate whether binary search is worthwhile here. (The
        // transitions array is sorted by match_byte() value.)
        for t in self.transitions() {
            if t.match_byte() == b {
                return Some(*t);
            }
        }
        None
    }
    #[allow(dead_code)]
    fn deep_show(&self, prefix: &str, dic: &Level) {
        if self.match_string_offset() != INVALID_STRING_OFFSET {
            let match_string = dic.string_at_offset(self.match_string_offset());
            println!("{}match: {}", prefix, str::from_utf8(match_string).unwrap());
        }
        for t in self.transitions() {
            println!("{}{} ->", prefix, t.match_byte() as char);
            dic.get_state(t.new_state_offset()).unwrap().deep_show(&(prefix.to_owned() + "  "), &dic);
        }
    }
}

fn lig_length(trail_byte: u8) -> usize {
    // This is only called on valid UTF-8 where we already know trail_byte
    // must be >= 0x80.
    // Ligature lengths:       ff   fi   fl   ffi  ffl  long-st  st
    const LENGTHS: [u8; 7] = [ 2u8, 2u8, 2u8, 3u8, 3u8, 2u8,     2u8 ];
    if trail_byte > 0x86 {
        return 1;
    }
    LENGTHS[trail_byte as usize - 0x80] as usize
}

// A hyphenation Level has a header followed by State records and packed string
// data. The total size of the slice depends on the number and size of the
// States and Strings it contains.
#[derive(Debug,Copy,Clone)]
struct Level<'a> {
    data: &'a [u8],
}

impl Level<'_> {
    // Accessors for Level header fields.
    fn state_data_base(&self) -> usize {
        u32::from_le_bytes(*array_ref!(self.data, 0, 4)) as usize
    }
    fn string_data_base(&self) -> usize {
        u32::from_le_bytes(*array_ref!(self.data, 4, 4)) as usize
    }
    fn nohyphen_string_offset(&self) -> usize {
        u16::from_le_bytes(*array_ref!(self.data, 8, 2)) as usize
    }
    fn nohyphen_count(&self) -> u16 {
        u16::from_le_bytes(*array_ref!(self.data, 10, 2))
    }
    fn lh_min(&self) -> usize {
        max(1, self.data[12] as usize)
    }
    fn rh_min(&self) -> usize {
        max(1, self.data[13] as usize)
    }
    fn clh_min(&self) -> usize {
        max(1, self.data[14] as usize)
    }
    fn crh_min(&self) -> usize {
        max(1, self.data[15] as usize)
    }
    fn word_boundary_mins(&self) -> (usize, usize, usize, usize) {
        (self.lh_min(), self.rh_min(), self.clh_min(), self.crh_min())
    }
    // Strings are represented as offsets from the Level's string_data_base.
    // This returns a byte slice referencing the string at a given offset,
    // or an empty slice if invalid.
    fn string_at_offset(&self, offset: usize) -> &'_ [u8] {
        assert!(offset != INVALID_STRING_OFFSET);
        let string_base = self.string_data_base() as usize + offset;
        let len = self.data[string_base] as usize;
        self.data.get(string_base + 1 .. string_base + 1 + len).unwrap()
    }
    // The nohyphen field is actually a string that contains multiple NUL-
    // separated substrings; return them as a vector of individual strings.
    fn nohyphen(&self) -> Option<Vec<&[u8]>> {
        let string_offset = self.nohyphen_string_offset();
        if string_offset == INVALID_STRING_OFFSET {
            None
        } else {
            Some(self.string_at_offset(string_offset as usize).split(|&b| b == 0).collect())
        }
    }
    // States are represented as an offset from the Level's state_data_base.
    // This returns the State at a given offset, or None if invalid.
    fn get_state(&self, offset: usize) -> Option<State> {
        if offset == INVALID_STATE_OFFSET {
            return None;
        }
        assert!(offset < self.string_data_base() - self.state_data_base());
        let base = self.state_data_base();
        let state_header = State {
            data: &self.data[base + offset .. base + offset + 8],
        };
        let length = if state_header.is_extended() { 12 } else { 8 } + 4 * state_header.num_transitions() as usize;
        Some(State{ data: &self.data[base + offset .. base + offset + length] })
    }
    fn find_hyphen_values(&self, word: &str, values: &mut [u8], lh_min: usize, rh_min: usize) {
        // Bail out immediately if the word is too short to hyphenate.
        let char_count = word.chars().count();
        if char_count < lh_min + rh_min {
            return;
        }
        let prep_word = ".".to_string() + word + ".";
        let start_state = self.get_state(0);
        let mut st = start_state;
        for i in 0 .. prep_word.len() {
            let b = prep_word.as_bytes()[i];
            loop {
                if st.is_none() {
                    st = start_state;
                    break;
                }
                let state = st.unwrap();
                let tr = state.transition_for(b);
                if tr.is_some() {
                    st = self.get_state(tr.unwrap().new_state_offset());
                    if st.is_some() {
                        let state = st.unwrap();
                        let match_offset = state.match_string_offset();
                        if match_offset != INVALID_STRING_OFFSET {
                            if state.is_extended() {
                                panic!("not yet implemented");
                            } else {
                                let match_str = self.string_at_offset(match_offset);
                                let offset = i + 1 - match_str.len();
                                assert!(offset + match_str.len() <= prep_word.len());
                                for j in 0 .. match_str.len() {
                                    let index = offset + j;
                                    if index >= lh_min && index <= word.len() - rh_min {
                                        // lh_min and rh_min are guaranteed to be >= 1,
                                        // so this will not try to access outside values[].
                                        if match_str[j] - b'0' > values[index - 1] {
                                            values[index - 1] = match_str[j] - b'0';
                                        }
                                    }
                                }
                            }
                        }
                    }
                    break;
                }
                st = self.get_state(state.fallback_state());
            }
        }
        // If the word was not purely ASCII, the use of lh_min and rh_min above
        // may not have correctly excluded enough positions in the UTF-8 string,
        // so we need to fix things up here.
        if char_count < word.len() {
            let mut index = 0;
            let mut count = 0;
            let word_bytes = word.as_bytes();
            // Handle lh_min
            while count < lh_min - 1 {
                let byte = word_bytes[index];
                if byte < 0x80 {
                    values[index] = 0;
                    index += 1;
                } else if byte == 0xEF && word_bytes[index + 1] == 0xAC {
                    count += lig_length(word_bytes[index + 2]);
                    values[index] = 0;
                    values[index + 1] = 0;
                    values[index + 2] = 0;
                    index += 3;
                    continue;
                } else {
                    values[index] = 0;
                    index += 1;
                    while index < word_bytes.len() && (word_bytes[index] & 0xC0) == 0x80  {
                        values[index] = 0;
                        index += 1;
                    }
                }
                count += 1;
            }
            // Handle rh_min
            count = 0;
            index = word.len();
            while count < rh_min {
                index -= 1;
                let byte = word_bytes[index];
                values[index] = 0;
                if byte < 0x80 {
                    count += 1;
                    continue;
                }
                if byte >= 0xC0 {
                    continue;
                }
                if byte == 0xEF && word_bytes[index + 1] == 0xAC {
                    count += lig_length(word_bytes[index + 2]);
                    continue;
                }
                count += 1;
            }
        }
    }
}

trait HyphenatorImpl {
    fn num_levels(&self) -> u32;
    fn level(&self, i: u32) -> Level;
}

impl HyphenatorImpl for &[u8] {
    fn num_levels(&self) -> u32 {
        u32::from_le_bytes(*array_ref!(self, 0, 4))
    }
    fn level(&self, i: u32) -> Level {
        let file_size = self.len() as usize;
        let offset = u32::from_le_bytes(*array_ref!(self, (4 + 4 * i) as usize, 4)) as usize;
        let limit = if i == self.num_levels() - 1 {
            file_size
        } else {
            u32::from_le_bytes(*array_ref!(self, (4 + 4 * i + 4) as usize, 4)) as usize
        };
        Level {
            data: &self[offset .. limit]
        }
    }
}

pub trait Hyphenator {
    fn find_hyphen_values(&self, word: &str, values: &mut [u8]);
    fn hyphenate_word(&self, word: &str, hyphchar: char) -> String;
}

impl Hyphenator for &[u8] {
    fn hyphenate_word(&self, word: &str, hyphchar: char) -> String {
        let mut values: Vec<u8> = vec![0; word.len()];
        self.find_hyphen_values(word, &mut values);
        let mut result = word.to_string();
        for i in (0 .. word.len()).rev() {
            if (values[i] & 1) == 1 {
                result.insert(i + 1, hyphchar);
            }
        }
        result
    }
    fn find_hyphen_values(&self, word: &str, values: &mut [u8]) {
        values.iter_mut().for_each(|x| *x = 0);
        let top_level = self.level(0);
        let (lh_min, rh_min, clh_min, crh_min) = top_level.word_boundary_mins();
        if word.len() < lh_min + rh_min {
            return;
        }
        top_level.find_hyphen_values(word, values, lh_min, rh_min);
        // Subsequent levels are applied to fragments between potential breaks
        // already found:
        for l in 1 .. self.num_levels() {
            let mut begin = 0;
            let mut lh = lh_min;
            let level = self.level(l);
            for i in lh - 1 .. word.len() - rh_min {
                if (values[i] & 1) == 1 || (begin > 0 && i == word.len() - 1) {
                    if i > begin {
                        values[begin .. i].iter_mut().for_each(|x| *x = 0);
                        level.find_hyphen_values(&word[begin .. i + 1],
                                                 &mut values[begin .. i + 1],
                                                 lh, crh_min);
                    }
                    begin = i + 1;
                    lh = clh_min;
                }
            }
            if begin == 0 {
                level.find_hyphen_values(word, values, lh_min, rh_min);
            } else if begin < word.len() {
                level.find_hyphen_values(&word[begin .. word.len()],
                                         &mut values[begin .. word.len()],
                                         clh_min, rh_min);
            }
        }
        let nohyph = &top_level.nohyphen();
        if nohyph.is_some() {
            for i in lh_min .. word.len() - rh_min + 1 {
                if (values[i - 1] & 1) == 1 {
                    for nh in nohyph.as_ref().unwrap() {
                        if i + nh.len() <= word.len() && *nh == &word.as_bytes()[i .. i + nh.len()] {
                            values[i - 1] = 0;
                            break;
                        }
                        if nh.len() <= i && *nh == &word.as_bytes()[i - nh.len() .. i] {
                            values[i - 1] = 0;
                            break;
                        }
                    }
                }
            }
        }
    }
}

impl Hyphenator for Mmap {
    fn find_hyphen_values(&self, word: &str, values: &mut [u8]) {
        self.deref().find_hyphen_values(word, values)
    }
    fn hyphenate_word(&self, word: &str, hyphchar: char) -> String {
        self.deref().hyphenate_word(word, hyphchar)
    }
}

pub fn load_file(dic_path: &str) -> Option<Mmap> {
    let file = match File::open(dic_path) {
        Err(_) => return None,
        Ok(file) => file,
    };
    let dic = match unsafe { Mmap::map(&file) } {
        Err(_) => return None,
        Ok(mmap) => mmap,
    };
    Some(dic)
}

// C-callable function to load a hyphenation dictionary; returns null on failure.
// `path` must be a valid UTF-8 string, or it will panic!
#[no_mangle]
pub extern "C" fn load_hyphenation(path: *const c_char) -> *const Mmap {
    let path_str = match unsafe { CStr::from_ptr(path) }.to_str() {
        Ok(str) => str,
        Err(_) => return std::ptr::null(),
    };
    let hyph = Box::new(match load_file(path_str) {
        Some(dic) => dic,
        _ => return std::ptr::null(),
    });
    Box::into_raw(hyph)
}

// C-callable function to free a hyphenation dictionary loaded by load_hyphenation.
#[no_mangle]
pub extern "C" fn free_hyphenation(hyph_ptr: *mut Mmap) {
    unsafe { Box::from_raw(hyph_ptr) };
}

// C-callable function to find hyphenation values for a word.
// Caller must supply the `hyphens` output buffer for results.
// **NOTE** that the `hyphens` buffer must be at least `word_len` elements long.
// Returns true on success; false if word is not valid UTF-8 or output buffer too small.
#[no_mangle]
pub extern "C" fn find_hyphen_values(dic: &Mmap, word: *const c_char, word_len: u32,
                                     hyphens: *mut u8, hyphens_len: u32) -> bool {
    if word_len > hyphens_len {
        return false;
    }
    let word_str = match str::from_utf8(unsafe { slice::from_raw_parts(word as *const u8, word_len as usize) } ) {
        Ok(word) => word,
        Err(_) => return false,
    };
    let hyphen_buf = unsafe { slice::from_raw_parts_mut(hyphens, hyphens_len as usize) };
    dic.find_hyphen_values(word_str, hyphen_buf);
    true
}
