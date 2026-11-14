//! Repairs an EPUB's table of contents when the book's own leaves out most of its chapters.
//!
//! Conversion services rebuild a book's navigation from its headings, and a book whose chapter titles are styled
//! paragraphs rather than headings comes out with almost none: Excession lists only its title and its epilogue across
//! 104 chapters, and The Arabs lists the headings of its notes, so every chapter entry lands in the notes. Two sources
//! in the book itself say where the chapters are. Many books carry a printed contents page whose links are right even
//! when the navigation is not, and many publishers class their chapter titles (`ct` for a chapter title, `cst` for its
//! subtitle). The printed page replaces the navigation only when it covers the book far better, and chapter titles are
//! added only where no entry already stands, so a book with good navigation keeps it.

use std::{
	collections::HashMap,
	io::{Read, Seek},
};

use roxmltree::{Document as XmlDocument, Node, ParsingOptions};
use zip::ZipArchive;

use super::{
	href::{resolve_href, split_href},
	spine::SectionMeta,
	toc::compute_nav_offset,
};
use crate::{
	document::{DocumentBuffer, TocItem},
	parser::{is_external_url, util::xml::collect_element_text},
	util::{
		text::{collapse_whitespace, trim_string},
		zip::read_zip_entry_by_name,
	},
};

/// Classes publishers give a chapter's or a part's title, and the front and back matter headings beside them.
const TITLE_CLASSES: &[&str] = &["ct", "cn", "pt", "ptn", "fmh", "bmh", "bmhT"];
/// Classes for the line that completes a title ("2" then "Not Invented Here").
const SUBTITLE_CLASSES: &[&str] = &["cst", "pst"];
/// Elements a contents entry sits in as its whole text; anything else around a link is inline.
const BLOCKS: &[&str] = &["p", "li", "div", "td", "dt", "dd", "h1", "h2", "h3", "h4", "h5", "h6", "section", "nav"];
/// Words that open a part's title, under which the chapters after it nest.
const PART_WORDS: &[&str] = &["part", "book", "volume"];
/// The share of the book, in tenths, that may go without an entry before the printed contents page is consulted.
const GAP_TENTHS: usize = 3;
/// Fewest entries a printed contents page must have to be taken for one.
const MIN_PRINTED_ENTRIES: usize = 3;
/// A longer line is prose that happened to be classed, not a title.
const MAX_TITLE_CHARS: usize = 200;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum TitleRole {
	Title,
	Subtitle,
}

/// A classed chapter title as conversion found it: its role, where it starts, and its `path#id`.
pub(super) struct ChapterTitle {
	pub(super) role: TitleRole,
	pub(super) offset: usize,
	pub(super) reference: String,
}

/// The ids of the elements a spine item's markup classes as a title or subtitle, in order. A plain scan of the
/// markup, run on every spine item while it is in memory; it only looks for class attributes. An element without an
/// id of its own is placed by the first element inside it, which is how the books that use these classes write it.
pub(super) fn title_ids(content: &str) -> Vec<(TitleRole, String)> {
	let mut found = Vec::new();
	let mut rest = content;
	while let Some(at) = rest.find("class=") {
		let after = &rest[at + 6..];
		let Some(quote) = after.chars().next().filter(|c| *c == '"' || *c == '\'') else {
			rest = after;
			continue;
		};
		let value_end = after[1..].find(quote).map_or(after.len(), |end| end + 1);
		let value = &after[1..value_end];
		let role = value.split_ascii_whitespace().find_map(|class| {
			if TITLE_CLASSES.contains(&class) {
				Some(TitleRole::Title)
			} else if SUBTITLE_CLASSES.contains(&class) {
				Some(TitleRole::Subtitle)
			} else {
				None
			}
		});
		let tag_start = rest[..at].rfind('<').unwrap_or(0);
		let tag_end = after.find('>').map_or(rest.len(), |end| at + 6 + end + 1);
		if let Some(role) = role {
			let id = attribute_id(&rest[tag_start..tag_end]).or_else(|| {
				let inner = rest[tag_end..].trim_start();
				inner
					.strip_prefix('<')
					.filter(|tag| !tag.starts_with('/'))
					.and_then(|tag| tag.find('>').and_then(|end| attribute_id(&tag[..end])))
			});
			if let Some(id) = id {
				found.push((role, id.to_string()));
			}
		}
		rest = &rest[tag_end.max(at + 6)..];
	}
	found
}

fn attribute_id(tag: &str) -> Option<&str> {
	let mut search = tag;
	while let Some(at) = search.find("id=") {
		let before = search[..at].chars().next_back();
		let after = &search[at + 3..];
		if before.is_some_and(char::is_whitespace)
			&& let Some(quote) = after.chars().next().filter(|c| *c == '"' || *c == '\'')
		{
			return after[1..].find(quote).map(|end| &after[1..=end]).filter(|id| !id.is_empty());
		}
		search = after;
	}
	None
}

/// The book's table of contents, repaired from its printed contents page and its classed chapter titles where its
/// own leaves most of the book out.
pub(super) fn repair_toc<R: Read + Seek>(
	archive: &mut ZipArchive<R>,
	buffer: &DocumentBuffer,
	sections: &[SectionMeta],
	id_positions: &HashMap<String, usize>,
	titles: &[ChapterTitle],
	toc: Vec<TocItem>,
) -> Vec<TocItem> {
	let length = buffer.total_display_len();
	let mut toc = toc;
	let own_gap = largest_gap(&toc, length);
	if own_gap * 10 > length * GAP_TENTHS
		&& let Some(printed) = best_printed_contents(archive, buffer, sections, id_positions)
	{
		let printed_gap = largest_gap(&printed, length);
		if printed_gap * 2 < own_gap {
			tracing::debug!(own_gap, printed_gap, entries = printed.len(), "epub toc replaced by printed contents");
			toc = printed;
		}
	}
	let added = add_missing_titles(&mut toc, buffer, titles);
	if added > 0 {
		tracing::debug!(added, "epub toc gained classed chapter titles");
	}
	toc
}

/// The longest stretch of the book, in display units, that no entry starts inside.
fn largest_gap(toc: &[TocItem], length: usize) -> usize {
	let mut offsets = Vec::new();
	collect_offsets(toc, &mut offsets);
	offsets.push(length);
	offsets.sort_unstable();
	let mut previous = 0;
	let mut largest = 0;
	for offset in offsets {
		largest = largest.max(offset.saturating_sub(previous));
		previous = offset;
	}
	largest
}

fn collect_offsets(items: &[TocItem], out: &mut Vec<usize>) {
	for item in items {
		out.push(item.offset);
		collect_offsets(&item.children, out);
	}
}

/// Line lookups over the converted text, by display offset.
struct Lines<'a> {
	buffer: &'a DocumentBuffer,
}

impl Lines<'_> {
	fn index(&self, offset: usize) -> usize {
		let char_index = self.buffer.char_index_for_display(offset);
		self.buffer.newline_positions().partition_point(|newline| *newline < char_index)
	}

	fn starts_line(&self, offset: usize) -> bool {
		let char_index = self.buffer.char_index_for_display(offset);
		char_index == 0 || self.buffer.newline_positions().binary_search(&(char_index - 1)).is_ok()
	}

	fn text(&self, line: usize) -> &str {
		let newlines = self.buffer.newline_positions();
		let start = if line == 0 { 0 } else { newlines.get(line - 1).map_or(usize::MAX, |newline| newline + 1) };
		if start == usize::MAX {
			return "";
		}
		let end = newlines.get(line).copied().unwrap_or_else(|| self.buffer.char_count());
		self.buffer.slice(self.buffer.byte_index_for_char(start)..self.buffer.byte_index_for_char(end)).trim()
	}
}

/// The run of contents links, among every spine item's, that leaves the least of the book uncovered.
fn best_printed_contents<R: Read + Seek>(
	archive: &mut ZipArchive<R>,
	buffer: &DocumentBuffer,
	sections: &[SectionMeta],
	id_positions: &HashMap<String, usize>,
) -> Option<Vec<TocItem>> {
	let lines = Lines { buffer };
	let length = buffer.total_display_len();
	let mut best: Option<(usize, Vec<TocItem>)> = None;
	for section in sections {
		let Ok(content) = read_zip_entry_by_name(archive, &section.path) else {
			continue;
		};
		if content.matches("<a ").count() < MIN_PRINTED_ENTRIES {
			continue;
		}
		let Ok(doc) =
			XmlDocument::parse_with_options(&content, ParsingOptions { allow_dtd: true, ..Default::default() })
		else {
			continue;
		};
		for run in contents_runs(&doc, &section.path, sections, id_positions, &lines) {
			let toc = nest_parts(run);
			let gap = largest_gap(&toc, length);
			if best.as_ref().is_none_or(|(best_gap, _)| gap < *best_gap) {
				best = Some((gap, toc));
			}
		}
	}
	best.map(|(_, toc)| toc)
}

/// One link of a printed contents page, with the class it sits under for nesting.
struct Entry {
	item: TocItem,
	class: String,
}

/// Each unbroken run of contents links in a spine item: links that are the whole text of their line, point inside
/// the book at the start of a line, and go forward through it. Note references fail the line test (they point into a
/// sentence) and index entries the whole-text test, so neither is taken for a contents page.
fn contents_runs(
	doc: &XmlDocument<'_>,
	path: &str,
	sections: &[SectionMeta],
	id_positions: &HashMap<String, usize>,
	lines: &Lines<'_>,
) -> Vec<Vec<Entry>> {
	let mut runs = Vec::new();
	let mut run: Vec<Entry> = Vec::new();
	for link in doc.descendants().filter(|node| node.is_element() && node.tag_name().name() == "a") {
		let Some(href) = link.attribute("href").or_else(|| link.attribute(("http://www.w3.org/1999/xlink", "href")))
		else {
			continue;
		};
		let entry = contents_entry(link, href, path, sections, id_positions, lines);
		match entry {
			Some(entry) if run.last().is_none_or(|last| entry.item.offset > last.item.offset) => run.push(entry),
			Some(entry) => {
				runs.push(std::mem::take(&mut run));
				run.push(entry);
			}
			None => runs.push(std::mem::take(&mut run)),
		}
	}
	runs.push(run);
	runs.retain(|run| run.len() >= MIN_PRINTED_ENTRIES);
	runs
}

fn contents_entry(
	link: Node<'_, '_>,
	href: &str,
	path: &str,
	sections: &[SectionMeta],
	id_positions: &HashMap<String, usize>,
	lines: &Lines<'_>,
) -> Option<Entry> {
	if is_external_url(href) {
		return None;
	}
	let reference = if href.starts_with('#') { format!("{path}{href}") } else { resolve_href(path, href) };
	let (target, _) = split_href(&reference);
	if !sections.iter().any(|section| section.path == target) {
		return None;
	}
	let name = node_text(link);
	if name.is_empty() || name.chars().count() > MAX_TITLE_CHARS {
		return None;
	}
	let block = link.ancestors().skip(1).find(|node| node.is_element() && BLOCKS.contains(&node.tag_name().name()))?;
	if node_text(block) != name {
		return None;
	}
	let offset = compute_nav_offset(&reference, sections, id_positions);
	if !lines.starts_line(offset) {
		return None;
	}
	let class = link.ancestors().skip(1).find_map(|node| node.attribute("class")).unwrap_or_default().to_string();
	Some(Entry { item: TocItem::new(name, reference, offset), class })
}

fn node_text(node: Node<'_, '_>) -> String {
	trim_string(&collapse_whitespace(&collect_element_text(node)))
}

/// A printed contents page lists parts and their chapters one after another, told apart only by their styling. A part
/// ("Part 2 - Not Invented Here") takes the entries after it as its chapters, for as long as they keep the styling of
/// the first; the next part, or an entry styled otherwise (an epilogue after the last part), ends it.
fn nest_parts(entries: Vec<Entry>) -> Vec<TocItem> {
	let mut toc: Vec<TocItem> = Vec::new();
	let mut chapter_class: Option<String> = None;
	let mut in_part = false;
	for entry in entries {
		if is_part(&entry.item.name) {
			toc.push(entry.item);
			in_part = true;
			chapter_class = None;
			continue;
		}
		if in_part && chapter_class.as_ref().is_none_or(|class| *class == entry.class) {
			chapter_class = Some(entry.class);
			if let Some(part) = toc.last_mut() {
				part.children.push(entry.item);
			}
			continue;
		}
		in_part = false;
		toc.push(entry.item);
	}
	toc
}

fn is_part(name: &str) -> bool {
	let mut words = name.split_whitespace();
	let first = words.next().unwrap_or_default().to_lowercase();
	PART_WORDS.contains(&first.as_str()) && words.next().is_some()
}

/// Adds every classed title no entry stands near, nesting it among the entries where its place in the book falls.
/// Returns how many were added.
///
/// A title whose name an entry beside it already carries, or already lands on, is a second look at that entry, not a
/// missing one. When the entry lands on that name, the title is a second page for it (a story's title page, then its
/// half-title after its contents list) and is left out. When the entry lands on other text, the book's markup sent it astray (The Arabs points its Index
/// at the notes before it), and the entry is moved to the title.
fn add_missing_titles(toc: &mut Vec<TocItem>, buffer: &DocumentBuffer, titles: &[ChapterTitle]) -> usize {
	let lines = Lines { buffer };
	let mut covered: Vec<usize> = Vec::new();
	collect_offsets(toc, &mut covered);
	let mut covered: Vec<usize> = covered.into_iter().map(|offset| lines.index(offset)).collect();
	covered.sort_unstable();
	let mut added = 0;
	for group in title_groups(titles, &lines) {
		let near = covered.partition_point(|line| *line + 2 < group.first_line);
		if covered.get(near).is_some_and(|line| *line <= group.last_line + 1) {
			continue;
		}
		let mut paths = Vec::new();
		entry_paths(toc, &mut Vec::new(), &mut paths);
		paths.sort_by_key(|(offset, _)| *offset);
		let after = paths.partition_point(|(offset, _)| *offset <= group.offset);
		let neighbours = [after.checked_sub(1), Some(after)];
		let namesake = neighbours.into_iter().flatten().filter_map(|index| paths.get(index)).find(|(offset, path)| {
			same_name(&entry_mut(toc, path).name, &group.first_text)
				|| same_name(lines.text(lines.index(*offset)), &group.first_text)
		});
		if let Some((offset, path)) = namesake {
			if !same_name(lines.text(lines.index(*offset)), &group.first_text) {
				let entry = entry_mut(toc, path);
				entry.offset = group.offset;
				entry.reference = group.reference;
			}
			continue;
		}
		insert_by_offset(toc, TocItem::new(group.name, group.reference, group.offset));
		added += 1;
	}
	added
}

fn same_name(a: &str, b: &str) -> bool {
	collapse_whitespace(a).trim().to_lowercase() == collapse_whitespace(b).trim().to_lowercase()
}

/// Every entry's offset with the child indices that lead to it.
fn entry_paths(items: &[TocItem], prefix: &mut Vec<usize>, out: &mut Vec<(usize, Vec<usize>)>) {
	for (index, item) in items.iter().enumerate() {
		prefix.push(index);
		out.push((item.offset, prefix.clone()));
		entry_paths(&item.children, prefix, out);
		prefix.pop();
	}
}

fn entry_mut<'a>(items: &'a mut [TocItem], path: &[usize]) -> &'a mut TocItem {
	let (first, rest) = path.split_first().expect("an entry path is never empty");
	let item = &mut items[*first];
	if rest.is_empty() { item } else { entry_mut(&mut item.children, rest) }
}

struct TitleGroup {
	name: String,
	/// The first line's text alone, which is what an entry for the same title would be called.
	first_text: String,
	reference: String,
	offset: usize,
	first_line: usize,
	last_line: usize,
}

/// Titles on neighbouring lines read as one ("2" and "Not Invented Here", or a chapter number and its name). A group
/// needs a title among its lines, so a subtitle alone, which some books use for epigraph sources, is not taken.
fn title_groups(titles: &[ChapterTitle], lines: &Lines<'_>) -> Vec<TitleGroup> {
	let mut sorted: Vec<&ChapterTitle> = titles.iter().collect();
	sorted.sort_by_key(|title| title.offset);
	let mut groups: Vec<(Vec<usize>, bool, &ChapterTitle)> = Vec::new();
	for title in sorted {
		let line = lines.index(title.offset);
		let is_title = title.role == TitleRole::Title;
		match groups.last_mut() {
			Some((group_lines, has_title, _)) if group_lines.last().is_some_and(|last| line <= last + 1) => {
				if !group_lines.contains(&line) {
					group_lines.push(line);
				}
				*has_title |= is_title;
			}
			_ => groups.push((vec![line], is_title, title)),
		}
	}
	groups
		.into_iter()
		.filter(|(_, has_title, _)| *has_title)
		.filter_map(|(group_lines, _, first)| {
			let mut name = String::new();
			let first_text = lines.text(group_lines[0]).to_string();
			for line in &group_lines {
				let text = lines.text(*line);
				if text.is_empty() {
					continue;
				}
				if !name.is_empty() {
					name.push_str(if name.ends_with(['.', ':', '!', '?']) { " " } else { ". " });
				}
				name.push_str(text);
			}
			(!name.is_empty() && name.chars().count() <= MAX_TITLE_CHARS).then(|| TitleGroup {
				name,
				first_text,
				reference: first.reference.clone(),
				offset: first.offset,
				first_line: group_lines[0],
				last_line: *group_lines.last().unwrap_or(&group_lines[0]),
			})
		})
		.collect()
}

/// Puts `item` after the last entry starting before it, inside that entry when its children already reach past it.
fn insert_by_offset(items: &mut Vec<TocItem>, item: TocItem) {
	let index = items.partition_point(|existing| existing.offset <= item.offset);
	if index > 0 {
		let previous = &mut items[index - 1];
		if previous.children.first().is_some_and(|child| child.offset <= item.offset) {
			insert_by_offset(&mut previous.children, item);
			return;
		}
	}
	items.insert(index, item);
}

#[cfg(test)]
mod tests;
