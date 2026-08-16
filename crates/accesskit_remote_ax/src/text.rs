//! Turning an element's text into AccessKit text runs.
//!
//! A consumer reads text through `TextRun` children, not through a string on
//! the container: the UIA Text pattern needs a role in `supports_text_ranges`
//! with at least one `TextRun` child before it will resolve a range at all.
//!
//! **Three offset systems meet here, and confusing them silently misplaces the
//! caret.** AX reports selection as a `CFRange` in *UTF-16 code units*, because
//! that is what `NSRange` has always meant. AccessKit's `character_index`
//! counts entries in `character_lengths`, which is one entry per *character*
//! (Unicode scalar). And each entry is a length in *UTF-8 bytes*. A caret past
//! any emoji or CJK text lands in the wrong place unless all three are kept
//! apart, so the conversions live here as pure functions with tests over the
//! astral planes rather than being written inline at the call site.

use accesskit::{NodeId, Node, Role, TextPosition, TextSelection};

/// The most characters one element gets per-character geometry for.
///
/// Geometry is the only read here that cannot be batched: each character needs
/// its own `AXBoundsForRange` call. A run's *own* rectangle is one call, which
/// is the part AX makes cheap — AT-SPI needed a call per code point simply to
/// reach that. Above this cap an element still carries text and a caret, just
/// no per-character rectangles, which degrades a magnifier rather than a
/// reader.
pub const MAX_GEOMETRY_CHARS: usize = 512;

/// Text longer than this is not mirrored.
///
/// A screen reader cannot usefully consume a megabyte in one node, and the
/// per-character arrays below are proportional to it. Matches the AT-SPI
/// source's cap.
pub const MAX_TEXT_CHARS: usize = 65_536;

/// Where one run sits in the element's text, so a caret offset can be resolved
/// back to a run and an index within it.
pub struct RunLayout {
    pub id: NodeId,
    /// Character offset of this run's first character within the whole text.
    pub start: usize,
    /// Number of characters in this run.
    pub len: usize,
}

/// Truncates to [`MAX_TEXT_CHARS`] characters, on a character boundary.
pub fn clamp(text: &str) -> &str {
    match text.char_indices().nth(MAX_TEXT_CHARS) {
        Some((index, _)) => &text[..index],
        None => text,
    }
}

/// Where each run starts and how many characters it holds.
///
/// **A `TextRun` is one visual line, not one paragraph.** AccessKit gives a run
/// a single bounding rectangle and a one-dimensional `character_positions`
/// array, so it has nowhere to put a second line's vertical offset. Emitting a
/// wrapped paragraph as one run therefore draws its later lines on top of its
/// earlier ones — measured through UIA: x correctly restarted at the wrap
/// point while every character reported the same y, so a magnifier following
/// the text jumped back to line one midway through.
///
/// Hard newlines always split. Wrapped lines can only be found from geometry,
/// and are: a character whose rectangle sits on a different row than its
/// predecessor begins a new run. Without geometry only hard lines are known,
/// which is the correct degradation — the text is still complete and the caret
/// still lands in the right place, only the rectangles are absent anyway.
fn line_spans(text: &str, geometry: Option<&Geometry>) -> Vec<(usize, usize)> {
    let count = text.chars().count();
    if count == 0 {
        // An empty field still needs somewhere for its caret to sit.
        return vec![(0, 0)];
    }
    let mut spans = Vec::new();
    let mut start = 0usize;
    let mut previous_was_newline = false;
    for (index, character) in text.chars().enumerate() {
        let wrapped = index > start && starts_new_visual_line(geometry, index);
        if (previous_was_newline || wrapped) && index > start {
            spans.push((start, index - start));
            start = index;
        }
        previous_was_newline = character == '\n';
    }
    spans.push((start, count - start));
    // Text ending in a newline gets a final empty run, so an end-of-document
    // caret has a run to sit in.
    if previous_was_newline {
        spans.push((count, 0));
    }
    spans
}

/// Whether the character at `index` sits on a lower row than the one before it.
///
/// Compared against half the taller of the two characters, so ordinary
/// baseline differences within a line do not read as a wrap while a real line
/// break always does.
fn starts_new_visual_line(geometry: Option<&Geometry>, index: usize) -> bool {
    let Some(geometry) = geometry else {
        return false;
    };
    let (Some(previous), Some(current)) = (
        geometry.characters.get(index - 1),
        geometry.characters.get(index),
    ) else {
        return false;
    };
    let tolerance = previous.3.max(current.3) * 0.5;
    (current.1 - previous.1).abs() > tolerance.max(1.0)
}

/// Splits text into one run per hard line.
///
/// Used where no geometry is available, and by the tests that pin newline
/// handling. The trailing newline stays with its run, and text ending in a
/// newline gets a final empty run.
pub fn split_runs(text: &str) -> Vec<&str> {
    let mut runs = Vec::new();
    let mut rest = text;
    loop {
        match rest.find('\n') {
            Some(index) => {
                runs.push(&rest[..=index]);
                rest = &rest[index + 1..];
            }
            None => {
                runs.push(rest);
                return runs;
            }
        }
    }
}

/// UTF-8 byte length of each character in the run.
///
/// This is what `character_lengths` means to AccessKit, and it is *not* the
/// same as a UTF-16 length: an emoji is 4 bytes here and 2 UTF-16 units on the
/// wire from AX.
pub fn character_lengths(run: &str) -> Vec<u8> {
    run.chars().map(|c| c.len_utf8() as u8).collect()
}

/// Index of the first character of each word in the run.
///
/// Indices are into [`character_lengths`], and the array degrades to empty if
/// any index exceeds a `u8`, which is all AccessKit stores.
pub fn word_starts(run: &str) -> Vec<u8> {
    let mut starts = Vec::new();
    let mut previous_was_space = true;
    for (index, character) in run.chars().enumerate() {
        let is_space = character.is_whitespace();
        if previous_was_space && !is_space {
            match u8::try_from(index) {
                Ok(index) => starts.push(index),
                Err(_) => return Vec::new(),
            }
        }
        previous_was_space = is_space;
    }
    starts
}

/// Converts a character index into the UTF-16 offset AX expects.
///
/// The inverse of [`utf16_to_char_index`], and needed because every AX text
/// range — selection, and the geometry queries below — is measured in UTF-16
/// code units while everything on the AccessKit side counts characters.
pub fn char_to_utf16_offset(text: &str, char_index: usize) -> usize {
    text.chars().take(char_index).map(char::len_utf16).sum()
}

/// Converts a UTF-16 offset, as AX reports selection, into a character index.
///
/// Offsets beyond the text clamp to its length rather than failing: AX and the
/// mirror can disagree momentarily when text changes between two reads, and a
/// caret at the end is a far better answer than no caret at all.
pub fn utf16_to_char_index(text: &str, utf16_offset: usize) -> usize {
    let mut utf16 = 0usize;
    for (index, character) in text.chars().enumerate() {
        if utf16 >= utf16_offset {
            return index;
        }
        utf16 += character.len_utf16();
    }
    text.chars().count()
}

/// Resolves a character offset within the whole text to a position in a run.
///
/// An offset on a run boundary lands at the start of the *following* run,
/// matching where a caret appears when it moves off the end of a line.
pub fn position(layout: &[RunLayout], offset: usize) -> Option<TextPosition> {
    for run in layout {
        if offset < run.start + run.len {
            return Some(TextPosition {
                node: run.id,
                character_index: offset.saturating_sub(run.start),
            });
        }
    }
    // Past every run: the end of the last one, which is where an
    // end-of-document caret belongs.
    layout.last().map(|run| TextPosition {
        node: run.id,
        character_index: run.len,
    })
}

/// Builds the `TextRun` children of a text element.
///
/// Returns the runs and their layout, the latter so a caret offset can be
/// resolved afterwards. `id_for_run` supplies a stable id per run index — run
/// ids must survive a re-walk exactly as element ids do, or every keystroke
/// would replace the whole paragraph rather than update it.
pub fn build_runs(
    text: &str,
    geometry: Option<&Geometry>,
    mut id_for_run: impl FnMut(usize) -> NodeId,
) -> (Vec<(NodeId, Node)>, Vec<RunLayout>) {
    let text = clamp(text);
    let mut nodes = Vec::new();
    let mut layout = Vec::new();
    let chars: Vec<char> = text.chars().collect();
    for (index, (start, len)) in line_spans(text, geometry).into_iter().enumerate() {
        let run: String = chars[start..start + len].iter().collect();
        let id = id_for_run(index);
        let mut node = Node::new(Role::TextRun);
        node.set_value(run.as_str());
        let lengths = character_lengths(&run);
        node.set_character_lengths(lengths);
        let words = word_starts(&run);
        if !words.is_empty() {
            node.set_word_starts(words);
        }
        // **Required for any geometry to be usable.** `accesskit_consumer`
        // needs four properties to produce a rectangle — bounds, character
        // positions, character widths and direction — and returns an empty
        // vector if *any* is absent. Omitting this one made every range query
        // answer zero rectangles while the other three were computed, paid for
        // and delivered intact: a silent loss at the last step.
        //
        // Always left-to-right for now. AX exposes no per-run direction, and
        // the AT-SPI source could only ever read a widget-level one; deriving
        // real bidirectional runs is unbuilt on both. Declaring LTR is wrong
        // for RTL text and is still strictly better than declaring nothing,
        // which loses the geometry for everyone.
        node.set_text_direction(accesskit::TextDirection::LeftToRight);
        if let Some(geometry) = geometry {
            apply_geometry(&mut node, geometry, start, len);
        }
        nodes.push((id, node));
        layout.push(RunLayout { id, start, len });
    }
    (nodes, layout)
}

/// Per-character rectangles for an element's whole text, window-relative and
/// in character order.
pub struct Geometry {
    /// One entry per character: `(x, y, width, height)`.
    pub characters: Vec<(f64, f64, f64, f64)>,
}

/// Attaches one run's slice of the geometry to its node.
///
/// AccessKit wants the run's own bounds plus, *relative to those bounds*, the
/// horizontal position and width of each character. A run whose characters
/// disagree in count with its text gets no geometry at all rather than
/// mismatched arrays — a caret placed from a misaligned array is worse than a
/// caret with no rectangle.
fn apply_geometry(node: &mut Node, geometry: &Geometry, start: usize, len: usize) {
    let Some(chars) = geometry.characters.get(start..start + len) else {
        return;
    };
    if chars.is_empty() {
        return;
    }
    // The run's rectangle is the union of its characters'. A newline reports a
    // zero-width box, which contributes its position but no extent.
    let left = chars.iter().map(|c| c.0).fold(f64::INFINITY, f64::min);
    let top = chars.iter().map(|c| c.1).fold(f64::INFINITY, f64::min);
    let right = chars.iter().map(|c| c.0 + c.2).fold(f64::NEG_INFINITY, f64::max);
    let bottom = chars.iter().map(|c| c.1 + c.3).fold(f64::NEG_INFINITY, f64::max);
    if !left.is_finite() || !top.is_finite() || right <= left {
        return;
    }
    node.set_bounds(accesskit::Rect {
        x0: left,
        y0: top,
        x1: right,
        y1: bottom,
    });
    let positions: Vec<f32> = chars.iter().map(|c| (c.0 - left) as f32).collect();
    let widths: Vec<f32> = chars.iter().map(|c| c.2 as f32).collect();
    node.set_character_positions(positions);
    node.set_character_widths(widths);
}

/// Builds the container's selection from AX's UTF-16 range.
///
/// A zero-length range is a caret, which AccessKit represents as a degenerate
/// selection — anchor equal to focus.
pub fn selection(
    text: &str,
    layout: &[RunLayout],
    utf16_start: usize,
    utf16_len: usize,
) -> Option<TextSelection> {
    let start = utf16_to_char_index(text, utf16_start);
    let end = utf16_to_char_index(text, utf16_start + utf16_len);
    Some(TextSelection {
        anchor: position(layout, start)?,
        focus: position(layout, end)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout_of(text: &str) -> Vec<RunLayout> {
        let mut next = 100u64;
        build_runs(text, None, |_| {
            next += 1;
            NodeId(next)
        })
        .1
    }

    #[test]
    fn runs_are_lines_and_keep_their_newline() {
        assert_eq!(split_runs("one\ntwo"), vec!["one\n", "two"]);
        assert_eq!(split_runs("single"), vec!["single"]);
    }

    #[test]
    fn a_caret_at_the_very_end_always_has_a_run_to_sit_in() {
        // Text ending in a newline, and empty text, are the two cases where a
        // naive split leaves the caret homeless.
        assert_eq!(split_runs("line\n"), vec!["line\n", ""]);
        assert_eq!(split_runs(""), vec![""]);
    }

    #[test]
    fn character_lengths_are_utf8_bytes_per_character() {
        assert_eq!(character_lengths("abc"), vec![1, 1, 1]);
        // Not UTF-16 units: an emoji is one character of four bytes.
        assert_eq!(character_lengths("a😀b"), vec![1, 4, 1]);
        assert_eq!(character_lengths("日本"), vec![3, 3]);
    }

    /// **The conversion that misplaces a caret if it is wrong.** AX counts
    /// UTF-16 units, AccessKit counts characters, and an emoji is two of the
    /// former and one of the latter.
    #[test]
    fn utf16_offsets_convert_across_the_astral_planes() {
        // "a😀b": UTF-16 is [a][😀 hi][😀 lo][b], characters are [a][😀][b].
        let text = "a😀b";
        assert_eq!(utf16_to_char_index(text, 0), 0);
        assert_eq!(utf16_to_char_index(text, 1), 1, "start of the emoji");
        assert_eq!(utf16_to_char_index(text, 3), 2, "after the surrogate pair");
        assert_eq!(utf16_to_char_index(text, 4), 3, "end of the text");

        // BMP multi-byte characters are one UTF-16 unit each, so offsets track.
        let cjk = "日本語";
        assert_eq!(utf16_to_char_index(cjk, 2), 2);
        assert_eq!(utf16_to_char_index(cjk, 3), 3);

        // Past the end clamps rather than failing: the two sides can disagree
        // for an instant when text changes between reads.
        assert_eq!(utf16_to_char_index(text, 999), 3);
    }

    #[test]
    fn a_position_resolves_into_the_right_run() {
        // "ab\ncd" is runs ["ab\n", "cd"], characters 0..3 and 3..5.
        let layout = layout_of("ab\ncd");
        assert_eq!(layout.len(), 2);
        let first = layout[0].id;
        let second = layout[1].id;

        assert_eq!(position(&layout, 0).unwrap(), TextPosition { node: first, character_index: 0 });
        assert_eq!(position(&layout, 2).unwrap(), TextPosition { node: first, character_index: 2 });
        // The boundary belongs to the next line, which is where the caret goes
        // when it moves off the end of one.
        assert_eq!(position(&layout, 3).unwrap(), TextPosition { node: second, character_index: 0 });
        // Past the end sits at the end of the last run.
        assert_eq!(position(&layout, 99).unwrap(), TextPosition { node: second, character_index: 2 });
    }

    #[test]
    fn a_caret_is_a_degenerate_selection() {
        let layout = layout_of("hello");
        let caret = selection("hello", &layout, 2, 0).unwrap();
        assert_eq!(caret.anchor, caret.focus, "a caret has no extent");
        assert_eq!(caret.focus.character_index, 2);
    }

    #[test]
    fn a_real_selection_keeps_its_direction() {
        let layout = layout_of("hello world");
        let selected = selection("hello world", &layout, 6, 5).unwrap();
        assert_eq!(selected.anchor.character_index, 6);
        assert_eq!(selected.focus.character_index, 11);
    }

    #[test]
    fn a_selection_measured_in_utf16_lands_on_the_right_characters() {
        // The case that would silently break: selecting "b" after an emoji.
        let text = "a😀b";
        let layout = layout_of(text);
        // UTF-16: a=0, emoji=1..3, b=3. Selecting b is start 3, length 1.
        let selected = selection(text, &layout, 3, 1).unwrap();
        assert_eq!(selected.anchor.character_index, 2, "b is the third character");
        assert_eq!(selected.focus.character_index, 3);
    }

    #[test]
    fn runs_carry_their_text_and_per_character_data() {
        let (nodes, layout) = build_runs("hi there", None, |index| NodeId(index as u64 + 1));
        assert_eq!(nodes.len(), 1);
        let (id, node) = &nodes[0];
        assert_eq!(*id, NodeId(1));
        assert_eq!(node.role(), Role::TextRun);
        assert_eq!(node.value(), Some("hi there"));
        assert_eq!(node.character_lengths().len(), 8);
        assert_eq!(node.word_starts(), &[0, 3], "two words");
        assert_eq!(layout[0].len, 8);
    }

    fn geometry(chars: &[(f64, f64, f64, f64)]) -> Geometry {
        Geometry { characters: chars.to_vec() }
    }

    #[test]
    fn character_positions_are_relative_to_their_own_run() {
        // Two lines, the second lower down. Each run's positions must be
        // measured from that run's own left edge, not the element's.
        let geo = geometry(&[
            (10.0, 0.0, 8.0, 16.0),
            (18.0, 0.0, 4.0, 16.0),
            (10.0, 16.0, 9.0, 16.0),
        ]);
        let (nodes, _) = build_runs("a\nb", Some(&geo), |i| NodeId(i as u64 + 1));
        assert_eq!(nodes.len(), 2);

        let first = &nodes[0].1;
        assert_eq!(first.character_positions(), Some(&[0.0f32, 8.0][..]), "offset from this run's left");
        assert_eq!(first.character_widths(), Some(&[8.0f32, 4.0][..]));
        let bounds = first.bounds().expect("a run with geometry has bounds");
        assert_eq!((bounds.x0, bounds.y0, bounds.x1), (10.0, 0.0, 22.0));

        let second = &nodes[1].1;
        assert_eq!(second.character_positions(), Some(&[0.0f32][..]), "the second line restarts at zero");
        assert_eq!(second.bounds().expect("bounds").y0, 16.0, "and sits lower");
    }

    /// **Regression.** Every range query returned zero rectangles because this
    /// one property was missing, while bounds, positions and widths were all
    /// computed and delivered. `accesskit_consumer` requires all four.
    #[test]
    fn every_run_declares_a_text_direction() {
        let geo = geometry(&[(0.0, 0.0, 8.0, 16.0), (8.0, 0.0, 8.0, 16.0)]);
        for source in [None, Some(&geo)] {
            let (nodes, _) = build_runs("ab", source, |i| NodeId(i as u64 + 1));
            assert_eq!(
                nodes[0].1.text_direction(),
                Some(accesskit::TextDirection::LeftToRight),
                "a run without a direction yields no rectangles at all"
            );
        }
    }

    /// **Regression: wrapped text was drawn on top of itself.** A `TextRun` is
    /// one visual line — AccessKit gives it a single rectangle and a
    /// one-dimensional position array — so a wrapped paragraph emitted as one
    /// run has nowhere to put the second line's y, and a magnifier following
    /// the text jumps back to line one midway through.
    #[test]
    fn a_wrapped_line_becomes_its_own_run() {
        // "abcd" with no newline, wrapping after "ab": the third character
        // starts a new row.
        let geo = geometry(&[
            (0.0, 0.0, 8.0, 16.0),
            (8.0, 0.0, 8.0, 16.0),
            (0.0, 16.0, 8.0, 16.0),
            (8.0, 16.0, 8.0, 16.0),
        ]);
        let (nodes, layout) = build_runs("abcd", Some(&geo), |i| NodeId(i as u64 + 1));
        assert_eq!(nodes.len(), 2, "one run per visual line");
        assert_eq!(nodes[0].1.value(), Some("ab"));
        assert_eq!(nodes[1].1.value(), Some("cd"));

        // Each run sits on its own row, and positions restart within it.
        assert_eq!(nodes[0].1.bounds().unwrap().y0, 0.0);
        assert_eq!(nodes[1].1.bounds().unwrap().y0, 16.0);
        assert_eq!(nodes[1].1.character_positions(), Some(&[0.0f32, 8.0][..]));

        // The layout still spans the whole text, so caret offsets resolve.
        assert_eq!(layout[0].start, 0);
        assert_eq!(layout[1].start, 2);
        assert_eq!(position(&layout, 3).unwrap().node, nodes[1].0);
    }

    #[test]
    fn a_baseline_wobble_within_a_line_is_not_a_wrap() {
        // Mixed font sizes on one line shift y slightly; that must not split.
        let geo = geometry(&[
            (0.0, 0.0, 8.0, 16.0),
            (8.0, 2.0, 8.0, 14.0),
            (16.0, 0.0, 8.0, 16.0),
        ]);
        let (nodes, _) = build_runs("abc", Some(&geo), |i| NodeId(i as u64 + 1));
        assert_eq!(nodes.len(), 1, "a 2px shift is not a new line");
    }

    #[test]
    fn hard_newlines_still_split_without_geometry() {
        let (nodes, _) = build_runs("one\ntwo", None, |i| NodeId(i as u64 + 1));
        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[0].1.value(), Some("one\n"));
        assert_eq!(nodes[1].1.value(), Some("two"));
    }

    #[test]
    fn an_empty_field_and_a_trailing_newline_keep_a_run_for_the_caret() {
        let (nodes, layout) = build_runs("", None, |i| NodeId(i as u64 + 1));
        assert_eq!(nodes.len(), 1);
        assert_eq!(layout[0].len, 0);

        let (nodes, _) = build_runs("line\n", None, |i| NodeId(i as u64 + 1));
        assert_eq!(nodes.len(), 2, "the caret past the newline needs a run");
        assert_eq!(nodes[1].1.value(), Some(""));
    }

    #[test]
    fn text_without_geometry_still_carries_its_content() {
        // Over the cap, or an unanswering element, must cost a reader nothing
        // — only a magnifier.
        let (nodes, _) = build_runs("hello", None, |i| NodeId(i as u64 + 1));
        assert_eq!(nodes[0].1.value(), Some("hello"));
        assert!(nodes[0].1.bounds().is_none());
        assert!(nodes[0].1.character_positions().is_none());
    }

    #[test]
    fn a_geometry_that_does_not_match_the_text_is_declined() {
        // Misaligned arrays misplace every character after the gap, which is
        // worse than having none.
        let geo = geometry(&[(0.0, 0.0, 8.0, 16.0)]);
        let (nodes, _) = build_runs("abc", Some(&geo), |i| NodeId(i as u64 + 1));
        assert!(nodes[0].1.bounds().is_none(), "3 characters, 1 rectangle: refuse");
    }

    #[test]
    fn utf16_offsets_round_trip_through_character_indices() {
        // The geometry reads walk the text in UTF-16 units, so both
        // conversions must agree or every rectangle after an emoji is wrong.
        for text in ["abc", "a\u{1F600}b", "\u{65E5}\u{672C}\u{8A9E}"] {
            for index in 0..=text.chars().count() {
                let utf16 = char_to_utf16_offset(text, index);
                assert_eq!(utf16_to_char_index(text, utf16), index, "{text:?} at {index}");
            }
        }
    }

    #[test]
    fn overlong_text_is_clamped_on_a_character_boundary() {
        let long = "😀".repeat(MAX_TEXT_CHARS + 10);
        let clamped = clamp(&long);
        assert_eq!(clamped.chars().count(), MAX_TEXT_CHARS);
        // Still valid UTF-8, i.e. not cut through a multi-byte character.
        assert!(clamped.chars().all(|c| c == '😀'));
    }

    #[test]
    fn word_starts_degrade_rather_than_truncate() {
        // AccessKit stores these as u8, so a run with a word starting past 255
        // has no usable word data — reporting none is honest, reporting wrong
        // offsets is not.
        let long = format!("{}word", " ".repeat(300));
        assert!(word_starts(&long).is_empty());
        assert_eq!(word_starts("one two"), vec![0, 4]);
    }

    /// A two-line text element inside a window, in the shape the walk emits.
    ///
    /// The container's rectangle deliberately extends far below its text, as a
    /// real text view's does: a few lines in a tall window leave most of the
    /// element empty. That empty region is where hit-testing gets interesting.
    ///
    /// Characters are 8pt wide and 16pt tall, the first line at y 0 and the
    /// second at y 16, both starting at x 10.
    fn two_line_text_element() -> accesskit::TreeUpdate {
        let geo = geometry(&[
            (10.0, 0.0, 8.0, 16.0),  // 'a'
            (18.0, 0.0, 8.0, 16.0),  // 'b'
            (26.0, 0.0, 0.0, 16.0),  // the newline, zero width
            (10.0, 16.0, 8.0, 16.0), // 'c'
            (18.0, 16.0, 8.0, 16.0), // 'd'
        ]);
        let (runs, _) = build_runs("ab\ncd", Some(&geo), |index| NodeId(index as u64 + 10));

        let mut container = Node::new(Role::MultilineTextInput);
        container.set_children(runs.iter().map(|(id, _)| *id).collect::<Vec<_>>());
        container.set_bounds(accesskit::Rect { x0: 10.0, y0: 0.0, x1: 400.0, y1: 300.0 });

        let mut root = Node::new(Role::Window);
        root.set_children(vec![NodeId(2)]);
        root.set_bounds(accesskit::Rect { x0: 0.0, y0: 0.0, x1: 400.0, y1: 300.0 });

        let mut nodes = vec![(NodeId(1), root), (NodeId(2), container)];
        nodes.extend(runs);
        accesskit::TreeUpdate {
            nodes,
            tree: Some(accesskit::Tree::new(NodeId(1))),
            tree_id: accesskit::TreeId::ROOT,
            focus: NodeId(2),
        }
    }

    /// **The coordinate-space contract for hit-testing, checked against the
    /// real consumer.** A point handed to `text_position_at_point` is in the
    /// same space the node's own bounds are in — for this crate, window-relative
    /// points — and it is not offset by the element's origin first. Getting that
    /// wrong sends every hit test to the end of the document, which is exactly
    /// what a UIA `RangeFromPoint` reported from Windows.
    #[test]
    fn a_point_on_a_character_resolves_to_that_character() {
        let tree = accesskit_consumer::Tree::new(two_line_text_element(), true);
        let state = tree.state();
        let element = state
            .node_by_tree_local_id(NodeId(2), accesskit::TreeId::ROOT)
            .expect("the text element is in the consumer's tree");
        assert!(element.supports_text_ranges(), "a consumer will answer range queries");

        // The centre of each character, in window coordinates, must resolve to
        // that character's index across the whole element — including the
        // second visual line, whose run restarts its own positions at zero.
        for (x, y, expected, what) in [
            (14.0, 8.0, 0, "'a', first line"),
            (22.0, 8.0, 1, "'b', first line"),
            (14.0, 24.0, 3, "'c', second line"),
            (22.0, 24.0, 4, "'d', second line"),
        ] {
            let position = element.text_position_at_point(accesskit::Point::new(x, y));
            assert_eq!(position.to_global_usv_index(), expected, "{what}");
        }
    }

    /// **Not a defect, and the reason this test exists.** A point inside the
    /// element but past its last character resolves to the end of the document,
    /// because there is no character there. A probe aimed at the centre of a
    /// tall, mostly empty text view therefore reports the final offset for
    /// every point tried — which reads as a broken hit test from outside and is
    /// the consumer behaving as designed.
    #[test]
    fn a_point_in_the_empty_space_below_the_text_is_the_end_of_the_document() {
        let tree = accesskit_consumer::Tree::new(two_line_text_element(), true);
        let state = tree.state();
        let element = state
            .node_by_tree_local_id(NodeId(2), accesskit::TreeId::ROOT)
            .unwrap();

        // The centre of the element — well below two lines of text.
        let position = element.text_position_at_point(accesskit::Point::new(200.0, 150.0));
        assert!(position.is_document_end(), "past the last character");

        // Above and left of the first character is the other end.
        let position = element.text_position_at_point(accesskit::Point::new(0.0, -5.0));
        assert!(position.is_document_start());
    }
}
