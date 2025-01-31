use std::{
    collections::{hash_map::Entry::{Occupied, Vacant}, HashMap},
    fs::File,
    io::{BufWriter, Write},
};

use serde::Serialize;
use serde_json;

use crate::{
    app::{error::RuntimeError, progress::build_progress_bar_export, runtime::Config},
    exporters::exporter::{BalloonFormatter, Exporter, TextEffectFormatter, Writer},
};

use imessage_database::{
    error::{plist::PlistParseError, table::TableError},
    message_types::{
        app::AppMessage,
        app_store::AppStoreMessage,
        collaboration::CollaborationMessage,
        digital_touch::DigitalTouch,
        edited::EditedMessage,
        handwriting::HandwrittenMessage,
        music::MusicMessage,
        placemark::PlacemarkMessage,
        sticker::StickerSource,
        text_effects::{Animation, Style, TextEffect, Unit},
        url::URLMessage,
    },
    tables::{
        attachment::Attachment,
        messages::{
            models::{AttachmentMeta, TextAttributes},
            Message,
        },
        table::{Table, ORPHANED},
    },
};
use rusqlite::Error;

#[derive(Debug, Serialize)]
struct JSONMessage {
    id: i32,
    guid: String,
    text: Option<String>,
    service: Option<String>,
    sender: Option<String>,
    subject: Option<String>,
    date: i64,
    date_read: i64,
    date_delivered: i64,
    is_from_me: bool,
    is_read: bool,
    attachments: Vec<JSONAttachment>,
    edited_messages: Vec<JSONEditedMessage>,
}

#[derive(Debug, Serialize)]
struct JSONAttachment {
    guid: String,
    filename: Option<String>,
    mime_type: Option<String>,
    total_bytes: i64,
}

#[derive(Debug, Serialize)]
struct JSONEditedMessage {
    text: Option<String>,
    date: i64,
}

pub struct JSON<'a> {
    /// Data that is setup from the application's runtime
    pub config: &'a Config,
    /// Handles to files we want to write messages to
    /// Map of resolved chatroom file location to a buffered writer
    pub files: HashMap<String, (BufWriter<File>, bool)>, // (writer, is_first_message)
    /// Writer instance for orphaned messages
    pub orphaned: (BufWriter<File>, bool), // (writer, is_first_message)
}

impl<'a> Exporter<'a> for JSON<'a> {
    fn new(config: &'a Config) -> Result<Self, RuntimeError> {
        let mut orphaned = config.options.export_path.clone();
        orphaned.push(ORPHANED);
        orphaned.set_extension("json");
        let file = File::options()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&orphaned)
            .map_err(|err| RuntimeError::CreateError(err, orphaned))?;

        let mut orphaned_writer = BufWriter::new(file);
        write!(orphaned_writer, "[\n").map_err(|e| RuntimeError::DatabaseError(TableError::Messages(Error::ToSqlConversionFailure(Box::new(e)))))?;

        Ok(JSON {
            config,
            files: HashMap::new(),
            orphaned: (orphaned_writer, true),
        })
    }

    fn iter_messages(&mut self) -> Result<(), RuntimeError> {
        eprintln!(
            "Exporting to {} as json...",
            self.config.options.export_path.display()
        );

        // Set up progress bar
        let mut current_message = 0;
        let total_messages =
            Message::get_count(&self.config.db, &self.config.options.query_context)
                .map_err(RuntimeError::DatabaseError)?;
        let pb = build_progress_bar_export(total_messages);

        let mut statement =
            Message::stream_rows(&self.config.db, &self.config.options.query_context)
                .map_err(RuntimeError::DatabaseError)?;

        let messages = statement
            .query_map([], |row| Ok(Message::from_row(row)))
            .map_err(|err| RuntimeError::DatabaseError(TableError::Messages(err)))?;

        for message in messages {
            let mut msg = Message::extract(message).map_err(RuntimeError::DatabaseError)?;
            let _ = msg.generate_text(&self.config.db);

            let json = self.message_to_json(&msg)?;
            let json_str = serde_json::to_string(&json)
                .map_err(|e| RuntimeError::DatabaseError(TableError::Messages(Error::ToSqlConversionFailure(Box::new(e)))))?;

            if let Some(chat_id) = msg.chat_id {
                let chat = self
                    .config
                    .chatrooms
                    .get(&chat_id)
                    .ok_or(RuntimeError::DatabaseError(TableError::Messages(Error::QueryReturnedNoRows)))?;

                let mut path = self.config.options.export_path.clone();
                path.push(self.config.filename(chat));
                path.set_extension("json");

                let file_path = path.to_string_lossy().to_string();
                let (buf, is_first) = match self.files.get_mut(&file_path) {
                    Some(entry) => entry,
                    None => {
                        let file = File::options()
                            .write(true)
                            .create(true)
                            .truncate(true)
                            .open(&path)
                            .map_err(|err| RuntimeError::CreateError(err, path))?;

                        let mut buf = BufWriter::new(file);
                        write!(buf, "[\n").map_err(|e| RuntimeError::DatabaseError(TableError::Messages(Error::ToSqlConversionFailure(Box::new(e)))))?;
                        self.files.insert(file_path.clone(), (buf, true));
                        self.files.get_mut(&file_path).unwrap()
                    }
                };

                if !*is_first {
                    write!(buf, ",\n").map_err(|e| RuntimeError::DatabaseError(TableError::Messages(Error::ToSqlConversionFailure(Box::new(e)))))?;
                }
                write!(buf, "{}", json_str).map_err(|e| RuntimeError::DatabaseError(TableError::Messages(Error::ToSqlConversionFailure(Box::new(e)))))?;
                *is_first = false;
            } else {
                let (buf, is_first) = &mut self.orphaned;
                if !*is_first {
                    write!(buf, ",\n").map_err(|e| RuntimeError::DatabaseError(TableError::Messages(Error::ToSqlConversionFailure(Box::new(e)))))?;
                }
                write!(buf, "{}", json_str).map_err(|e| RuntimeError::DatabaseError(TableError::Messages(Error::ToSqlConversionFailure(Box::new(e)))))?;
                *is_first = false;
            }

            current_message += 1;
            if current_message % 99 == 0 {
                pb.set_position(current_message);
            }
        }
        pb.finish();

        eprintln!("Writing JSON footers...");
        for (buf, _) in self.files.values_mut() {
            write!(buf, "\n]").map_err(|e| RuntimeError::DatabaseError(TableError::Messages(Error::ToSqlConversionFailure(Box::new(e)))))?;
        }
        write!(self.orphaned.0, "\n]").map_err(|e| RuntimeError::DatabaseError(TableError::Messages(Error::ToSqlConversionFailure(Box::new(e)))))?;

        Ok(())
    }

    fn get_or_create_file(
        &mut self,
        _message: &Message,
    ) -> Result<&mut BufWriter<File>, RuntimeError> {
        // This is now handled directly in iter_messages
        unreachable!()
    }
}

impl<'a> JSON<'a> {
    fn message_to_json(&self, message: &Message) -> Result<JSONMessage, RuntimeError> {
        let mut attachments = Vec::new();
        let mut edited_messages = Vec::new();

        // Get attachments
        if message.has_attachments() {
            let message_attachments = Attachment::from_message(&self.config.db, message)
                .map_err(RuntimeError::DatabaseError)?;

            for attachment in message_attachments {
                attachments.push(JSONAttachment {
                    guid: attachment.rowid.to_string(),
                    filename: attachment.filename,
                    mime_type: attachment.mime_type,
                    total_bytes: attachment.total_bytes,
                });
            }
        }

        // Get edited messages
        if message.is_edited() {
            if let Some(edited) = &message.edited_parts {
                for part in &edited.parts {
                    for edit in &part.edit_history {
                        edited_messages.push(JSONEditedMessage {
                            text: edit.text.clone(),
                            date: edit.date,
                        });
                    }
                }
            }
        }

        Ok(JSONMessage {
            id: message.rowid,
            guid: message.guid.clone(),
            text: message.text.clone(),
            service: message.service.clone(),
            sender: message.handle_id.and_then(|id| self.config.participants.get(&id).cloned()),
            subject: message.subject.clone(),
            date: message.date,
            date_read: message.date_read,
            date_delivered: message.date_delivered,
            is_from_me: message.is_from_me,
            is_read: message.is_read,
            attachments,
            edited_messages,
        })
    }
}

// Implement required traits with minimal functionality since we don't need HTML-specific formatting
impl<'a> Writer<'a> for JSON<'a> {
    fn format_message(&self, _message: &Message, _indent_size: usize) -> Result<String, TableError> {
        Ok(String::new()) // Not used for JSON export
    }

    fn format_attachment(
        &self,
        _: &'a mut Attachment,
        _: &'a Message,
        _: &AttachmentMeta,
    ) -> Result<String, &'a str> {
        Ok(String::new())
    }

    fn format_sticker(&self, _: &'a mut Attachment, _: &Message) -> String {
        String::new()
    }

    fn format_app(
        &self,
        _: &'a Message,
        _: &mut Vec<Attachment>,
        _: &str,
    ) -> Result<String, PlistParseError> {
        Ok(String::new())
    }

    fn format_tapback(&self, _: &Message) -> Result<String, TableError> {
        Ok(String::new())
    }

    fn format_expressive(&self, _: &'a Message) -> &'a str {
        ""
    }

    fn format_announcement(&self, _: &'a Message) -> String {
        String::new()
    }

    fn format_shareplay(&self) -> &str {
        ""
    }

    fn format_shared_location(&self, _: &'a Message) -> &str {
        ""
    }

    fn format_edited(
        &self,
        _: &'a Message,
        _: &'a EditedMessage,
        _: usize,
        _: &str,
    ) -> Option<String> {
        None
    }

    fn format_attributes(&'a self, _: &'a str, _: &'a [TextAttributes]) -> String {
        String::new()
    }

    fn write_to_file(file: &mut BufWriter<File>, text: &str) -> Result<(), RuntimeError> {
        file.write_all(text.as_bytes())
            .map_err(|e| RuntimeError::DatabaseError(TableError::Messages(Error::ToSqlConversionFailure(Box::new(e)))))
    }
}

impl<'a> BalloonFormatter<&'a Message> for JSON<'a> {
    fn format_url(&self, _: &Message, _: &URLMessage, _: &'a Message) -> String {
        String::new()
    }

    fn format_music(&self, _: &MusicMessage, _: &'a Message) -> String {
        String::new()
    }

    fn format_collaboration(&self, _: &CollaborationMessage, _: &'a Message) -> String {
        String::new()
    }

    fn format_app_store(&self, _: &AppStoreMessage, _: &'a Message) -> String {
        String::new()
    }

    fn format_placemark(&self, _: &PlacemarkMessage, _: &'a Message) -> String {
        String::new()
    }

    fn format_handwriting(&self, _: &Message, _: &HandwrittenMessage, _: &'a Message) -> String {
        String::new()
    }

    fn format_digital_touch(&self, _: &Message, _: &DigitalTouch, _: &'a Message) -> String {
        String::new()
    }

    fn format_apple_pay(&self, _: &AppMessage, _: &'a Message) -> String {
        String::new()
    }

    fn format_fitness(&self, _: &AppMessage, _: &'a Message) -> String {
        String::new()
    }

    fn format_slideshow(&self, _: &AppMessage, _: &'a Message) -> String {
        String::new()
    }

    fn format_find_my(&self, _: &AppMessage, _: &'a Message) -> String {
        String::new()
    }

    fn format_check_in(&self, _: &AppMessage, _: &'a Message) -> String {
        String::new()
    }

    fn format_generic_app(
        &self,
        _: &AppMessage,
        _: &str,
        _: &mut Vec<Attachment>,
        _: &'a Message,
    ) -> String {
        String::new()
    }
}

impl<'a> TextEffectFormatter<'a> for JSON<'a> {
    fn format_effect(&'a self, text: &'a str, _: &'a TextEffect) -> std::borrow::Cow<'a, str> {
        text.into()
    }

    fn format_mention(&self, text: &str, _: &str) -> String {
        text.to_string()
    }

    fn format_link(&self, text: &str, _: &str) -> String {
        text.to_string()
    }

    fn format_otp(&self, text: &str) -> String {
        text.to_string()
    }

    fn format_conversion(&self, text: &str, _: &Unit) -> String {
        text.to_string()
    }

    fn format_styles(&self, text: &str, _: &[Style]) -> String {
        text.to_string()
    }

    fn format_animated(&self, text: &str, _: &Animation) -> String {
        text.to_string()
    }
} 