use humansize::{DECIMAL, format_size};
use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Direction, Layout, Rect},
    style::Style,
    symbols,
    text::ToLine as _,
    widgets::{Block, BorderType, Fill, Widget},
};

use crate::core::{Entry, MaybePair, StackAddr, TreeSlice};
use crate::forest::{key_range, partition};
use crate::state::AppState;

pub fn render_subtree(
    state: &mut AppState,
    addr: &StackAddr,
    area: Rect,
    buf: &mut Buffer,
    tree: TreeSlice,
    selection: &[usize],
) {
    if tree.is_empty() {
        return;
    }

    // Can't display useful information if area is too small
    if tree.len() > 1 && (area.height < 2 || area.width <= 2) {
        let head = selection.first();
        let color = tree.first().map(|(_, it)| it.color).unwrap_or_default();
        let style = Style::from(color);
        if tree.iter().any(|(k, _)| Some(k) == head) {
            Fill::new("▓").style(style).render(area, buf);
        } else {
            Fill::new(symbols::DOT).style(style).render(area, buf);
        }

        let addr = addr.push(tree.first().unwrap().0);

        if let Some(click) = &state.click_pos
            && area.contains(*click)
            && state.click_area.intersection(area) == area
        {
            state.click_area = area;
            state.click_addr.clear();
            for id in &addr {
                state.click_addr.push(id)
            }

            state.click_addr.reverse();
        }

        return;
    }

    if tree.len() == 1 {
        let (key, entry) = &tree[0];

        render_entry(state, addr, area, buf, *key, entry, selection);

        return;
    }

    match partition(tree) {
        MaybePair::One(entries) => {
            render_subtree(state, addr, area, buf, entries, selection);
            // Paragraph::new(format!("{entries:?}"))
            //     .centered()
            //     .render(area, buf);
        }
        MaybePair::Two(left, right) => {
            let l = key_range(left).map(|r| (r.end - r.start) as f32).unwrap();
            let r = key_range(right).map(|r| (r.end - r.start) as f32).unwrap();

            // Must interpolate multi-gigabytes down to u16 range
            let lr = (l + r) / 1E5;
            let l = (l / lr) as u16;
            let r = (r / lr) as u16;

            let direction = if area.width > area.height * 2 {
                Direction::Horizontal
            } else {
                Direction::Vertical
            };

            let mut layout = Layout::default()
                .direction(direction)
                .constraints(vec![Constraint::Fill(l), Constraint::Fill(r)])
                .split(area);

            // Ensure tiny left-overs are always represented even if it skews proportions
            if layout[1].width == 0 || layout[1].height == 0 {
                layout = Layout::default()
                    .direction(direction)
                    .constraints(vec![Constraint::Percentage(100), Constraint::Min(1)])
                    .split(area);
            }

            render_subtree(state, addr, layout[0], buf, left, selection);
            render_subtree(state, addr, layout[1], buf, right, selection);
        }
    }
}

pub fn render_entry(
    state: &mut AppState,
    addr: &StackAddr,
    area: Rect,
    buf: &mut Buffer,
    key: usize,
    entry: &Entry,
    selection: &[usize],
) {
    let Entry {
        path,
        size,
        subtree,
        is_group,
        nfiles,
        ..
    } = entry;

    let addr = addr.push(key);
    let title = path.file_name().unwrap_or_default();
    let display = title.display();

    if let Some(click) = &state.click_pos
        && area.contains(*click)
        && state.click_area.intersection(area) == area
    {
        state.click_area = area;
        state.click_addr.clear();
        for id in &addr {
            state.click_addr.push(id)
        }

        state.click_addr.reverse();
    }

    let (selected, selection) = if selection.first() == Some(&key) {
        (true, &selection[1..])
    } else {
        (false, [].as_slice())
    };

    let style = Style::from(entry.color);

    let mut block = Block::bordered()
        .title(display.to_line())
        .border_style(style);

    if area.height > 1 {
        // let mut a = addr.collect_vec();
        // a.reverse();
        // block = block.title_bottom(format!("{a:?}"));
        block = block.title_bottom(format_size(*size, DECIMAL));
    }

    if selected {
        block = block.border_type(BorderType::QuadrantInside);
    } else if *is_group {
        block = block.border_type(BorderType::Double);
    } else if subtree.is_empty() && *nfiles == 1 {
        block = block.border_type(BorderType::LightDoubleDashed);
    }

    let inner = block.inner(area);
    block.render(area, buf);
    if subtree.is_empty() {
        Fill::new(if selected { "▓" } else { "▒" })
            .style(style)
            .render(inner, buf);
    } else if inner.height > 2 || inner.width > 2 {
        render_subtree(state, &addr, inner, buf, subtree, selection);
    }
}
