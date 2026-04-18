mod auth;
mod detect;
mod error;
pub mod oauth;
mod reader;

pub use auth::{GDriveAuth, api_key, oauth_client_id, oauth_client_secret};
pub use detect::{extract_file_id, is_gdrive_url};
pub use error::GDriveError;
pub use reader::GoogleDriveReadAt;
