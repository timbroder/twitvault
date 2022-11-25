use eyre::Report;

use crate::storage::Storage;

#[derive(Debug)]
pub enum Message {
    Finished(Storage),
    Loading(String),
    Error(Report),
}