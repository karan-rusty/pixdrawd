use ratatui::style::Color;

pub struct Palette {
    pub colors: Vec<(Color, String)>,
    pub index: usize,
}

impl Palette {
    pub fn from_config(config: &crate::config::Palette) -> Self {
        let colors: Vec<(Color, String)> = config
            .colors
            .iter()
            .zip(config.names.iter())
            .map(|(c, n)| (*c, n.clone()))
            .collect();
        Self { colors, index: 0 }
    }

    pub fn current(&self) -> Color {
        self.colors[self.index].0
    }

    pub fn name(&self) -> &str {
        &self.colors[self.index].1
    }

    pub fn next(&mut self) {
        self.index = (self.index + 1) % self.colors.len();
    }

    pub fn prev(&mut self) {
        self.index = if self.index == 0 {
            self.colors.len() - 1
        } else {
            self.index - 1
        };
    }

    pub fn select(&mut self, n: usize) {
        if n < self.colors.len() {
            self.index = n;
        }
    }
}
