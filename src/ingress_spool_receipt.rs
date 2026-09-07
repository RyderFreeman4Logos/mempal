use std::{fs, io};

use crate::core::queue::PendingMessageStore;

use super::{IngressSpool, IngressSpoolError, read_record};

impl IngressSpool {
    pub(crate) fn contains_operation_id(
        &self,
        operation_id: &str,
    ) -> Result<bool, IngressSpoolError> {
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(IngressSpoolError::Io(error)),
        };
        for entry in entries {
            let path = entry.map_err(IngressSpoolError::Io)?.path();
            if !matches!(
                path.extension().and_then(|value| value.to_str()),
                Some("json" | "claim")
            ) {
                continue;
            }
            let request = match read_record(&path) {
                Ok(request) => request,
                Err(IngressSpoolError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
                    continue;
                }
                Err(IngressSpoolError::Decode { .. }) => continue,
                Err(error) => return Err(error),
            };
            if PendingMessageStore::idempotent_message_id(&request.kind, &request.idempotency_key)
                == operation_id
            {
                return Ok(true);
            }
        }
        Ok(false)
    }
}
