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
