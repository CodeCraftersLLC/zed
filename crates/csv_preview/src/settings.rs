use gpui::Styled as _;

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
    /// row grows to fit, so it draws every line.
    pub(crate) fn drawn_text(
        self,
        full: &ui::SharedString,
        single_line: &ui::SharedString,
    ) -> ui::SharedString {
        match self {
            Self::VariableList => full.clone(),
            Self::UniformList => single_line.clone(),
        }
    }

    /// The box a cell's text is drawn in. GPUI's ellipsis measures the whole string as one
    /// line, so it would cut the later lines of a variable row that fit the column; only a
    /// uniform row, which draws one flattened line, truncates.
    pub(crate) fn cell_wrapper(self) -> ui::Div {
        let wrapper = ui::div().size_full().whitespace_nowrap();
        match self {
            Self::VariableList => wrapper,
            Self::UniformList => wrapper.text_ellipsis(),
        }
    }
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
    use gpui::ParentElement as _;

    /// Draws `text` in a 60px-wide cell and returns the text GPUI laid out.
    fn drawn_in_narrow_cell(
        mechanism: RowRenderMechanism,
        text: &'static str,
    ) -> Vec<ui::SharedString> {
        let mut cx = gpui::TestAppContext::single();
        let cx = cx.add_empty_window();
        cx.draw(
            gpui::point(gpui::px(0.), gpui::px(0.)),
            gpui::size(gpui::px(60.), gpui::px(200.)),
            |_, _| {
                ui::div()
                    .w(gpui::px(60.))
                    .child(mechanism.cell_wrapper().child(text))
            },
        );
        cx.update(|window, _| window.text_system().texts_of_last_frame())
    }

    /// A variable row's second line fits the column; only the joined text is too wide.
    #[test]
    fn a_variable_row_keeps_every_line_of_a_cell_whose_joined_text_overflows() {
        let drawn = drawn_in_narrow_cell(RowRenderMechanism::VariableList, "aaaaaa\nbb");
        assert!(drawn.iter().any(|t| t.as_ref() == "bb"), "{drawn:?}");
    }

    /// A uniform row draws one flattened line, which still ends in an ellipsis when too wide.
    #[test]
    fn a_uniform_row_ellipsizes_a_flattened_cell_that_overflows() {
        let drawn = drawn_in_narrow_cell(RowRenderMechanism::UniformList, "aaaaaaaaaaaaaaaaaaaa");
        assert!(drawn.iter().any(|t| t.ends_with('…')), "{drawn:?}");
    }

    #[test]
    fn a_variable_row_draws_every_line_and_a_uniform_row_the_flattened_cell() {
        let full: ui::SharedString = "a\nb".into();
        let single_line: ui::SharedString = "a↵b".into();
        assert_eq!(
            RowRenderMechanism::VariableList.drawn_text(&full, &single_line),
            "a\nb"
        );
        assert_eq!(
            RowRenderMechanism::UniformList.drawn_text(&full, &single_line),
            "a↵b"
        );
    }
}
