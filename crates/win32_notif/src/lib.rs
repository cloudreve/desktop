#![cfg_attr(docsrs, feature(doc_cfg))]
#![allow(private_bounds)]

//! Win32 Notification
//!
//! This library implements UWP XML Toast Notification
//! This is a safe wrapper around the official WinRT apis
//!
//! # Example
//! ```ignore
//! use win32_notif::{
//!  notification::visual::progress::{Progress, ProgressValue},
//!  string, NotificationBuilder, ToastsNotifier,
//! };
//!
//! fn main() {
//!   let notifier = ToastsNotifier::new("Microsoft.Windows.Explorer").unwrap();
//!   let notif = NotificationBuilder::new()
//!     .visual(Progress::new(
//!       None,
//!       string!("Downloading..."),
//!       ProgressValue::BindTo("prog"),
//!       None,
//!     ))
//!     // Use the newest data binding method
//!     .value("prog", "0.3")
//!     .build(1, &notifier, "a", "ahq")
//!     .unwrap();
//!
//!   let _ = notif.show();
//!   loop {}
//! }
//! ```

#[macro_export]
///
/// Creates a reference to a value in notification
///
/// # Example
/// ```rust
/// use win32_notif::string;
///
/// fn main() {
///     let value = string!("status");
/// }
/// ```
macro_rules! string {
    ($($x:tt)*) => {
        format!($($x)*)
    };
}

#[cfg(target_os = "windows")]
mod structs;

use std::{error::Error, fmt::Display};

#[cfg(target_os = "windows")]
pub use structs::*;

#[cfg(target_os = "windows")]
macro_rules! from_impl {
  ($x:ty => $y:ident) => {
    impl From<$x> for NotifError {
      fn from(value: $x) -> Self {
        Self::$y(value)
      }
    }
  };
}

#[derive(Debug)]
pub enum NotifError {
  #[cfg(target_os = "windows")]
  WindowsCore(windows::core::Error),
  DurationTooLong,
  UnknownAndImpossible,
  #[cfg(not(target_os = "windows"))]
  UnsupportedPlatform,
}

impl Display for NotifError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    write!(f, "{:?}", self)
  }
}

impl Error for NotifError {}

#[cfg(target_os = "windows")]
from_impl!(windows::core::Error => WindowsCore);

#[cfg(not(target_os = "windows"))]
pub mod notification {
  pub mod actions {
    pub struct ActionButton;
    pub struct Input;
    pub mod input {
      pub struct Selection;
      impl Selection {
        pub fn new(_id: &str, _content: &str) -> Self {
          Self
        }
      }
    }

    impl ActionButton {
      pub fn create(_content: &str) -> Self {
        Self
      }

      pub fn with_id(self, _id: &str) -> Self {
        self
      }

      pub fn with_tooltip(self, _tooltip: &str) -> Self {
        self
      }
    }

    impl Input {
      #[allow(clippy::too_many_arguments)]
      pub fn create_selection_input(
        _id: &str,
        _title: &str,
        _hint: &str,
        _items: Vec<input::Selection>,
        _default: &str,
      ) -> Self {
        Self
      }
    }
  }

  pub mod visual {
    pub struct Image;
    pub enum Placement {
      AppLogoOverride,
    }
    pub struct Text;
    pub mod text {
      pub enum HintStyle {
        Title,
        Body,
      }
    }

    impl Image {
      pub fn create(_id: u8, _src: &str) -> Self {
        Self
      }

      pub fn with_placement(self, _placement: Placement) -> Self {
        self
      }
    }

    impl Text {
      pub fn create(_id: u8, _content: &str) -> Self {
        Self
      }

      pub fn with_align_center(self, _value: bool) -> Self {
        self
      }

      pub fn with_wrap(self, _value: bool) -> Self {
        self
      }

      pub fn with_style(self, _style: text::HintStyle) -> Self {
        self
      }
    }
  }
}

#[cfg(not(target_os = "windows"))]
pub struct ToastsNotifier;

#[cfg(not(target_os = "windows"))]
impl ToastsNotifier {
  pub fn new(_app: &str) -> Result<Self, NotifError> {
    Ok(Self)
  }
}

#[cfg(not(target_os = "windows"))]
pub struct NotificationBuilder;

#[cfg(not(target_os = "windows"))]
impl NotificationBuilder {
  pub fn new() -> Self {
    Self
  }

  pub fn visual<T>(self, _widget: T) -> Self {
    self
  }

  pub fn actions<T>(self, _actions: Vec<T>) -> Self {
    self
  }

  pub fn with_launch(self, _launch: &str) -> Self {
    self
  }

  pub fn build(
    self,
    _id: i32,
    _notifier: &ToastsNotifier,
    _tag: &str,
    _group: &str,
  ) -> Result<Notification, NotifError> {
    Ok(Notification)
  }
}

#[cfg(not(target_os = "windows"))]
pub struct Notification;

#[cfg(not(target_os = "windows"))]
impl Notification {
  pub fn show(&self) -> Result<(), NotifError> {
    Ok(())
  }
}
