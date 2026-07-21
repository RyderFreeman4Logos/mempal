use super::db_admission::{
    ADMISSION_RELEASE_MAX_ATTEMPTS, ADMISSION_RELEASE_RETRY_DELAY, ProfileDbAdmission,
};

impl Drop for ProfileDbAdmission {
    fn drop(&mut self) {
        // Bounded retry keeps a transient lock or unlink failure from leaking
        // admission capacity for the lifetime of this process.
        for attempt in 1..=ADMISSION_RELEASE_MAX_ATTEMPTS {
            match self.release() {
                Ok(_) => return,
                Err(error) if attempt < ADMISSION_RELEASE_MAX_ATTEMPTS => {
                    tracing::warn!(
                        %error,
                        admission_owner = %self.owner_identity(),
                        attempt,
                        "failed to release DB admission holder; will retry"
                    );
                    std::thread::sleep(ADMISSION_RELEASE_RETRY_DELAY);
                }
                Err(error) => {
                    tracing::error!(
                        %error,
                        admission_owner = %self.owner_identity(),
                        "failed to release DB admission holder after {} attempts; \
                         budget slot may leak until process exit",
                        ADMISSION_RELEASE_MAX_ATTEMPTS
                    );
                }
            }
        }
    }
}
