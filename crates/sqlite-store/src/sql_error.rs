use miden_client::store::StoreError;

pub(crate) trait SqlResultExt<T> {
    fn into_store_error(self) -> Result<T, StoreError>;
}

impl<T> SqlResultExt<T> for Result<T, rusqlite::Error> {
    fn into_store_error(self) -> Result<T, StoreError> {
        self.map_err(|value| match &value {
            rusqlite::Error::FromSqlConversionFailure(..)
            | rusqlite::Error::IntegralValueOutOfRange(..)
            | rusqlite::Error::InvalidColumnIndex(_)
            | rusqlite::Error::InvalidColumnType(..) => StoreError::ParsingError(value.to_string()),
            rusqlite::Error::InvalidParameterName(_)
            | rusqlite::Error::InvalidColumnName(_)
            | rusqlite::Error::StatementChangedRows(_)
            | rusqlite::Error::ExecuteReturnedResults
            | rusqlite::Error::InvalidQuery
            | rusqlite::Error::MultipleStatement
            | rusqlite::Error::InvalidParameterCount(..)
            | rusqlite::Error::QueryReturnedNoRows => StoreError::QueryError(value.to_string()),
            rusqlite::Error::SqliteFailure(error, _) => match error.code {
                rusqlite::ErrorCode::DatabaseBusy => StoreError::DatabaseBusy(value.to_string()),
                rusqlite::ErrorCode::DatabaseLocked => {
                    StoreError::DatabaseLocked(value.to_string())
                },
                rusqlite::ErrorCode::ConstraintViolation => {
                    StoreError::ConstraintViolation(value.to_string())
                },
                rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase => {
                    StoreError::DatabaseCorrupted(value.to_string())
                },
                _ => StoreError::DatabaseError(value.to_string()),
            },
            _ => StoreError::DatabaseError(value.to_string()),
        })
    }
}
