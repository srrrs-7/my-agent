//! What the user hands the agent for one turn.
//!
//! Mostly text, so every string type converts into it and a caller that has
//! only text never names this type. Images arrive through `@` references
//! (see [`super::references`]); they ride along as their own blocks of the
//! user message rather than as text, because each provider spells an image
//! differently and that is the adapters' business.

use agent_domain::model::image::Image;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UserInput {
    pub text: String,
    /// In the order attached; they follow the text in the message.
    pub images: Vec<Image>,
}

impl UserInput {
    pub fn with_images(text: impl Into<String>, images: Vec<Image>) -> Self {
        Self {
            text: text.into(),
            images,
        }
    }
}

impl From<String> for UserInput {
    fn from(text: String) -> Self {
        Self::with_images(text, Vec::new())
    }
}

impl From<&str> for UserInput {
    fn from(text: &str) -> Self {
        Self::from(text.to_string())
    }
}

impl From<&String> for UserInput {
    fn from(text: &String) -> Self {
        Self::from(text.clone())
    }
}
