use crate::app::DrawingApp;

impl DrawingApp {
    pub(crate) fn push_history(&mut self) {
        let pts = self.points.clone();
        self.history.push(pts);
        self.redo_stack.clear();
    }

    pub(crate) fn undo(&mut self) {
        if let Some(prev) = self.history.pop() {
            let replaced = std::mem::replace(&mut self.points, prev);
            self.redo_stack.push(replaced);
        }
    }

    pub(crate) fn redo(&mut self) {
        if let Some(next) = self.redo_stack.pop() {
            let replaced = std::mem::replace(&mut self.points, next);
            self.history.push(replaced);
        }
    }
}
