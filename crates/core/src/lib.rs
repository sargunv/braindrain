use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrainDrainState {
    pub title: String,
}

impl Default for BrainDrainState {
    fn default() -> Self {
        Self {
            title: "BrainDrain".to_owned(),
        }
    }
}

impl BrainDrainState {
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_title_is_app_name() {
        assert_eq!(BrainDrainState::default().title, "BrainDrain");
    }
}
