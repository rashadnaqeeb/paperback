use std::{fs::File, io::BufReader};

use super::*;
use crate::parser::epub::{
	package::{find_container_path, parse_package},
	spine::convert_spine_items,
	toc::build_epub_toc,
};

fn title(role: TitleRole, offset: usize) -> ChapterTitle {
	ChapterTitle { role, offset, reference: format!("ch.xhtml#t{offset}") }
}

fn names(items: &[TocItem]) -> Vec<String> {
	let mut out = Vec::new();
	flatten(items, 0, &mut out);
	out
}

fn flatten(items: &[TocItem], depth: usize, out: &mut Vec<String>) {
	for item in items {
		out.push(format!("{}{}@{}", "  ".repeat(depth), item.name, item.offset));
		flatten(&item.children, depth + 1, out);
	}
}

#[test]
fn title_ids_finds_classed_elements_and_places_a_bare_one_by_its_first_child() {
	let markup = r#"<section id="s1" class="ct"><p id="p1">II</p></section>
		<section class="ct"><p id="p2">III</p></section>
		<p id="p3" class="cst special">Not Invented Here</p>
		<p id="p4" class="cct">contents entry</p>
		<p id="p5" class='bmhT'>Index</p>
		<p class="ct">no id anywhere</p>"#;
	assert_eq!(
		title_ids(markup),
		vec![
			(TitleRole::Title, "s1".to_string()),
			(TitleRole::Title, "p2".to_string()),
			(TitleRole::Subtitle, "p3".to_string()),
			(TitleRole::Title, "p5".to_string()),
		]
	);
}

#[test]
fn attribute_id_does_not_take_another_attribute_ending_in_id() {
	assert_eq!(attribute_id(r#"<p data-id="no" id="yes">"#), Some("yes"));
	assert_eq!(attribute_id(r#"<p data-id="no">"#), None);
}

#[test]
fn the_largest_gap_counts_the_stretches_before_the_first_entry_and_after_the_last() {
	let toc = vec![TocItem::new("A".into(), String::new(), 10), TocItem::new("B".into(), String::new(), 30)];
	assert_eq!(largest_gap(&toc, 100), 70);
	assert_eq!(largest_gap(&[], 100), 100);
}

fn entry(name: &str, offset: usize, class: &str) -> Entry {
	Entry { item: TocItem::new(name.into(), String::new(), offset), class: class.into() }
}

#[test]
fn a_part_takes_the_chapters_after_it_until_the_styling_changes() {
	let toc = nest_parts(vec![
		entry("Dedication", 0, "cfmh"),
		entry("Part 1 - Outside Context Problem", 1, ""),
		entry("I", 2, "cct"),
		entry("II", 3, "cct"),
		entry("Part 2 - Not Invented Here", 4, ""),
		entry("I", 5, "cct"),
		entry("Epilogue", 6, "cbmh"),
	]);
	assert_eq!(
		names(&toc),
		vec![
			"Dedication@0",
			"Part 1 - Outside Context Problem@1",
			"  I@2",
			"  II@3",
			"Part 2 - Not Invented Here@4",
			"  I@5",
			"Epilogue@6"
		]
	);
}

#[test]
fn a_contents_page_without_parts_stays_flat() {
	let toc = nest_parts(vec![entry("Introduction", 0, "cfmh"), entry("CHAPTER 1", 1, ""), entry("Partings", 2, "")]);
	assert_eq!(names(&toc), vec!["Introduction@0", "CHAPTER 1@1", "Partings@2"]);
}

/// A buffer whose lines are `lines`, and the display offset each line starts at.
fn text_buffer(lines: &[&str]) -> (DocumentBuffer, Vec<usize>) {
	let mut starts = Vec::new();
	let mut text = String::new();
	for line in lines {
		starts.push(text.chars().count());
		text.push_str(line);
		text.push('\n');
	}
	(DocumentBuffer::with_content(text), starts)
}

#[test]
fn missing_chapter_titles_are_added_with_their_subtitles_and_covered_ones_are_not() {
	let (buffer, at) = text_buffer(&[
		"Airsphere",
		"Prose.",
		"More prose.",
		"Still more.",
		"1",
		"The Light of Ancient Mistakes",
		"Prose.",
		"Dirigible",
		"2",
	]);
	let mut toc = vec![
		TocItem::new("Airsphere".into(), String::new(), at[0]),
		TocItem::new("Dirigible".into(), String::new(), at[7]),
	];
	let titles = [
		title(TitleRole::Title, at[0]),
		title(TitleRole::Title, at[4]),
		title(TitleRole::Subtitle, at[5]),
		title(TitleRole::Title, at[8]),
	];
	assert_eq!(add_missing_titles(&mut toc, &buffer, &titles), 1, "the titles beside an entry are covered");
	assert_eq!(
		names(&toc),
		vec![
			"Airsphere@0".to_string(),
			format!("1. The Light of Ancient Mistakes@{}", at[4]),
			format!("Dirigible@{}", at[7])
		]
	);
}

#[test]
fn a_subtitle_alone_is_not_a_title() {
	let (buffer, at) = text_buffer(&["Start", "Prose.", "— Thucydides", "Prose."]);
	let mut toc = vec![TocItem::new("Start".into(), String::new(), 0)];
	assert_eq!(add_missing_titles(&mut toc, &buffer, &[title(TitleRole::Subtitle, at[2])]), 0);
}

#[test]
fn a_title_nests_inside_the_entry_whose_children_already_reach_past_it() {
	let mut part = TocItem::new("Part One".into(), String::new(), 0);
	part.children.push(TocItem::new("Chapter 1".into(), String::new(), 10));
	part.children.push(TocItem::new("Chapter 3".into(), String::new(), 30));
	let mut toc = vec![part, TocItem::new("Part Two".into(), String::new(), 40)];
	insert_by_offset(&mut toc, TocItem::new("Chapter 2".into(), String::new(), 20));
	insert_by_offset(&mut toc, TocItem::new("Part Three".into(), String::new(), 50));
	assert_eq!(
		names(&toc),
		vec!["Part One@0", "  Chapter 1@10", "  Chapter 2@20", "  Chapter 3@30", "Part Two@40", "Part Three@50"]
	);
}

#[test]
fn an_entry_sent_astray_moves_to_its_title_and_one_on_its_own_name_keeps_its_place() {
	let (buffer, at) = text_buffer(&["Notes", "Epilogue", "a note", "b note", "c note", "Index", "Abbasids, 23"]);
	let mut toc =
		vec![TocItem::new("Notes".into(), String::new(), at[0]), TocItem::new("Index".into(), String::new(), at[1])];
	assert_eq!(add_missing_titles(&mut toc, &buffer, &[title(TitleRole::Title, at[5])]), 0);
	assert_eq!(names(&toc), vec!["Notes@0".to_string(), format!("Index@{}", at[5])]);
	let (buffer, at) =
		text_buffer(&["The State of the Art", "CONTENTS", "1. Excuses", "2. Stranger", "The State of the Art"]);
	let mut toc = vec![TocItem::new("The State of the Art".into(), String::new(), at[4])];
	let titles = [title(TitleRole::Title, at[0]), title(TitleRole::Subtitle, at[1])];
	assert_eq!(add_missing_titles(&mut toc, &buffer, &titles), 0);
	assert_eq!(names(&toc), vec![format!("The State of the Art@{}", at[4])]);
}

/// A fixture book's table of contents as its navigation gives it, and as repaired, with the text each entry lands on.
fn own_and_repaired(name: &str) -> Option<(Vec<TocItem>, Vec<TocItem>, DocumentBuffer)> {
	let path = std::path::Path::new(&std::env::var("HOME").ok()?)
		.join("Library/Mobile Documents/com~apple~CloudDocs/books")
		.join(name);
	let file = File::open(&path).ok()?;
	let mut archive = ZipArchive::new(BufReader::new(file)).ok()?;
	let container = find_container_path(&mut archive).ok()?;
	let opf = read_zip_entry_by_name(&mut archive, &container).ok()?;
	let opf_dir = container.rfind('/').map_or(String::new(), |i| container[..i].to_string());
	let doc = XmlDocument::parse_with_options(&opf, ParsingOptions { allow_dtd: true, ..Default::default() }).ok()?;
	let package = doc.descendants().find(|n| n.tag_name().name() == "package")?;
	let (manifest, spine, nav, ncx, _) = parse_package(package, &opf_dir);
	let conversion = convert_spine_items(&path.to_string_lossy(), &manifest, &spine, false);
	let own =
		build_epub_toc(&mut archive, nav.as_deref(), ncx.as_deref(), &conversion.sections, &conversion.id_positions);
	let repaired = repair_toc(
		&mut archive,
		&conversion.buffer,
		&conversion.sections,
		&conversion.id_positions,
		&conversion.titles,
		own.clone(),
	);
	Some((own, repaired, conversion.buffer))
}

fn landing(buffer: &DocumentBuffer, item: &TocItem) -> String {
	let lines = Lines { buffer };
	lines.text(lines.index(item.offset)).to_string()
}

#[test]
fn excession_gets_its_twelve_parts_from_the_printed_contents_page() {
	let Some((own, toc, buffer)) = own_and_repaired("Excession_rashad_naqeeb.epub") else {
		return;
	};
	assert_eq!(own.len(), 2);
	let parts: Vec<&TocItem> = toc.iter().filter(|item| item.name.starts_with("Part ")).collect();
	assert_eq!(parts.len(), 12);
	assert_eq!(parts[1].name, "Part 2 - Not Invented Here");
	assert_eq!(landing(&buffer, parts[1]), "2");
	assert_eq!(parts.iter().map(|part| part.children.len()).sum::<usize>(), 91);
	assert!(parts.iter().all(|part| part.children.first().is_some_and(|first| first.name == "I")));
	assert_eq!(toc.last().map(|item| item.name.as_str()), Some("Epilogue"));
}

#[test]
fn the_arabs_chapters_land_in_the_chapters_and_not_in_the_notes() {
	let Some((own, toc, buffer)) = own_and_repaired("The_Arabs_rashad_naqeeb.epub") else {
		return;
	};
	assert_eq!(
		own.iter().find(|item| item.name == "Chapter 1").map(|item| landing(&buffer, item)),
		Some("Chapter 1".into())
	);
	let chapter = toc.iter().find(|item| item.name == "CHAPTER 1 - From Cairo to Istanbul").expect("chapter 1");
	assert_eq!(landing(&buffer, chapter), "CHAPTER 1");
	assert_eq!(toc.iter().filter(|item| item.name.starts_with("CHAPTER ")).count(), 14);
	let index: Vec<&TocItem> = toc.iter().filter(|item| item.name == "Index").collect();
	assert_eq!(index.len(), 1);
	assert_eq!(landing(&buffer, index[0]), "Index");
}

#[test]
fn look_to_windward_gains_its_sixteen_chapters_between_the_interludes() {
	let Some((own, toc, _)) = own_and_repaired("Look_To_Windward_rashad_naqeeb.epub") else {
		return;
	};
	assert_eq!(own.len(), 7);
	let names = names(&toc);
	let first = names.iter().position(|name| name.starts_with("1. The Light of Ancient Mistakes")).expect("chapter 1");
	assert!(names[first - 1].starts_with("Look To Windward@"), "{names:?}");
	assert!(names[first + 4].starts_with("Airsphere@"), "{names:?}");
	assert_eq!(names.iter().filter(|name| name.starts_with(|c: char| c.is_ascii_digit())).count(), 16);
}

#[test]
fn books_whose_navigation_covers_their_chapters_keep_it() {
	for name in [
		"The_Prize_rashad_naqeeb.epub",
		"War_and_Peace_rashad_naqeeb.epub",
		"Revelation_Space_rashad_naqeeb.epub",
		"Consider_Phlebas_rashad_naqeeb.epub",
		"Matter_rashad_naqeeb.epub",
		"Use_of_Weapons_rashad_naqeeb.epub",
		"The_Player_of_Games_rashad_naqeeb.epub",
		"The_Word_for_World_is_Forest_rashad_naqeeb.epub",
	] {
		let Some((own, toc, _)) = own_and_repaired(name) else {
			continue;
		};
		assert_eq!(names(&own), names(&toc), "{name}");
	}
}
