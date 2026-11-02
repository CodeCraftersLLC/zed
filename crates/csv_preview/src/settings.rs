use gpui::{ParentElement as _, Styled as _};

#[derive(Default, Clone, Copy, PartialEq)]
pub enum RowRenderMechanism {
    /// More correct for multiline content, but slower.
    #[allow(dead_code)] // Will be used when settings ui is added
    VariableList,
    /// Default behaviour for now while resizable columns are being stabilized.
    #[default]
    UniformList,
}

impl RowRenderMechanism {
    /// A uniform row has one fixed height, so a multi-line cell is drawn flattened; a variable
    /// row grows to fit, so it draws every line. Both are shared, never copied.
    pub(crate) fn drawn_lines<'a>(
        self,
        lines: &'a [ui::SharedString],
        single_line: &'a [ui::SharedString],
    ) -> &'a [ui::SharedString] {
        match self {
            Self::VariableList => lines,
            Self::UniformList => single_line,
        }
    }
}

/// The box a cell's text is drawn in. Each line is its own child because GPUI's ellipsis
/// measures one text run as a single line: a joined multi-line cell would be cut after its
/// first line, while one child per line cuts only the lines that are too wide.
pub(crate) fn cell_wrapper() -> ui::Div {
    ui::div().size_full().whitespace_nowrap().text_ellipsis()
}

/// The lines, one child each, for [`cell_wrapper`] to cut on its own.
pub(crate) fn cell_lines(lines: &[ui::SharedString]) -> ui::Div {
    ui::div().children(lines.iter().map(|line| ui::div().child(line.clone())))
}

#[derive(Default, Clone, Copy)]
pub enum VerticalAlignment {
    /// Align text to the top of cells
    #[default]
    Top,
    /// Center text vertically in cells
    Center,
}

#[derive(Default, Clone, Copy)]
pub enum RowIdentifiers {
    /// Show original line numbers from CSV file
    #[default]
    SrcLines,
    /// Show sequential row numbers starting from 1
    RowNum,
}

#[derive(Clone, Default)]
pub(crate) struct CsvPreviewSettings {
    pub(crate) rendering_with: RowRenderMechanism,
    pub(crate) vertical_alignment: VerticalAlignment,
    pub(crate) numbering_type: RowIdentifiers,
    pub(crate) show_debug_info: bool,
    #[cfg(feature = "dev-tools")]
    pub(crate) show_perf_metrics_overlay: bool,
    pub(crate) multiline_cells_enabled: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// Draws `text` in a 60px-wide cell and returns the text GPUI laid out.
    fn drawn_in_narrow_cell(text: &'static str) -> Vec<ui::SharedString> {
        let mut cx = gpui::TestAppContext::single();
        let cx = cx.add_empty_window();
        let lines: Vec<ui::SharedString> = text.split('\n').map(Into::into).collect();
        cx.draw(
            gpui::point(gpui::px(0.), gpui::px(0.)),
            gpui::size(gpui::px(60.), gpui::px(200.)),
            |_, _| {
                ui::div()
                    .w(gpui::px(60.))
                    .child(cell_wrapper().child(cell_lines(&lines)))
            },
        );
        cx.update(|window, _| window.text_system().texts_of_last_frame())
    }

    /// A variable row's second line fits the column; only the joined text is too wide.
    #[test]
    fn a_variable_row_keeps_every_line_of_a_cell_whose_joined_text_overflows() {
        let drawn = drawn_in_narrow_cell("aaaaaa\nbb");
        assert!(drawn.iter().any(|t| t.as_ref() == "bb"), "{drawn:?}");
    }

    /// A uniform row draws one flattened line, which still ends in an ellipsis when too wide.
    #[test]
    fn a_uniform_row_ellipsizes_a_flattened_cell_that_overflows() {
        let drawn = drawn_in_narrow_cell("aaaaaaaaaaaaaaaaaaaa");
        assert!(drawn.iter().any(|t| t.ends_with('…')), "{drawn:?}");
    }

    /// Each line of a variable row is cut on its own: a long line shows an ellipsis and the lines
    /// after it are still drawn.
    #[test]
    fn a_variable_row_ellipsizes_each_long_line_and_keeps_the_rest() {
        let drawn = drawn_in_narrow_cell("aaaaaaaaaaaaaaaaaaaa\nbb");
        assert!(drawn.iter().any(|t| t.ends_with('…')), "{drawn:?}");
        assert!(drawn.iter().any(|t| t.as_ref() == "bb"), "{drawn:?}");
    }

    /// Drawing hands out the cell's shared lines: nothing is split or copied per render.
    #[test]
    fn a_row_draws_the_cells_shared_lines_without_copying_them() {
        let lines: Arc<[ui::SharedString]> = Arc::from(["a".into(), "b".into()]);
        let single_line: Arc<[ui::SharedString]> = Arc::from(["a↵b".into()]);
        assert!(std::ptr::eq(
            RowRenderMechanism::VariableList.drawn_lines(&lines, &single_line),
            &*lines
        ));
        assert!(std::ptr::eq(
            RowRenderMechanism::UniformList.drawn_lines(&lines, &single_line),
            &*single_line
        ));
    }
}
