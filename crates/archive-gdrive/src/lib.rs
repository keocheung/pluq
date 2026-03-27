mod auth;
mod detect;
mod error;
mod reader;

pub use auth::api_key;
pub use detect::{extract_file_id, is_gdrive_url};
pub use error::GDriveError;
pub use reader::GoogleDriveReadAt;
