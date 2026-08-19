use thiserror::Error;

pub type Result<T, E = StoreError> = std::result::Result<T, E>;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Sqlx(#[from] sqlx::Error),

    #[error("migration error: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),

    #[error("cannot map row to domain type: {0}")]
    Mapping(String),

    #[error("type error: {0}")]
    Types(#[from] rgbpp_types::Error),
}

impl StoreError {
    pub fn mapping(reason: impl Into<String>) -> Self {
        StoreError::Mapping(reason.into())
    }
}
