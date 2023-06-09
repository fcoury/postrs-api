use axum::body::BoxBody;
use axum::response::{IntoResponse, Response};
use reqwest::StatusCode;
use tracing::error;

use crate::database::DatabaseError;
use crate::graph::GraphClientError;

pub enum AppError {
    GraphClient(GraphClientError),
    Database(DatabaseError),
    Other(anyhow::Error),
    BadRequest(String),
}

impl From<GraphClientError> for AppError {
    fn from(inner: GraphClientError) -> Self {
        AppError::GraphClient(inner)
    }
}

impl From<DatabaseError> for AppError {
    fn from(inner: DatabaseError) -> Self {
        AppError::Database(inner)
    }
}

impl From<anyhow::Error> for AppError {
    fn from(inner: anyhow::Error) -> Self {
        AppError::Other(inner)
    }
}

#[derive(Debug)]
pub struct CustomError {
    message: String,
    status: StatusCode,
}

impl CustomError {
    pub fn new(message: String, status: StatusCode) -> Self {
        Self { message, status }
    }
}

impl IntoResponse for CustomError {
    fn into_response(self) -> Response<BoxBody> {
        let message = self.message;
        let status = self.status;

        // Create a JSON response with the error message and the given status code
        let json = axum::Json(serde_json::json!({ "message": message }));
        let mut response = json.into_response();
        *response.status_mut() = status;
        response
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            AppError::GraphClient(GraphClientError::Request(status)) => {
                error!("Request error: {}", status);
                let message = match status {
                    StatusCode::UNAUTHORIZED => "Unauthorized".to_string(),
                    StatusCode::FORBIDDEN => "Forbidden".to_string(),
                    StatusCode::NOT_FOUND => "Not found".to_string(),
                    _ => "An error occurred while processing the request".to_string(),
                };
                (status, message)
            }
            AppError::Database(err) => {
                let message = err.to_string();
                error!("Database error: {:?}", err);
                (StatusCode::INTERNAL_SERVER_ERROR, message)
            }
            AppError::GraphClient(err) => {
                let message = err.to_string();
                error!("GraphClient error: {:?}", err);
                (StatusCode::INTERNAL_SERVER_ERROR, message)
            }
            AppError::Other(err) => {
                error!("Unknown error: {:?}", err);
                let message = err.to_string();
                (StatusCode::INTERNAL_SERVER_ERROR, message)
            }
            AppError::BadRequest(message) => (StatusCode::BAD_REQUEST, message),
        };

        let error_response = CustomError::new(message, status);
        error_response.into_response()
    }
}
