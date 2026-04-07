use actix_web::{HttpResponse, ResponseError};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("Not found")]
    NotFound,
    #[error("Forbidden")]
    Forbidden,
    #[error("Unauthorized")]
    Unauthorized,
    #[error("Bad request: {0}")]
    BadRequest(String),
    #[error("Database error: {0}")]
    Database(#[from] diesel::result::Error),
    #[error("Internal server error")]
    Internal(#[from] anyhow::Error),
}

impl ResponseError for AppError {
    fn error_response(&self) -> HttpResponse {
        match self {
            AppError::NotFound => HttpResponse::NotFound().body("404 Not Found"),
            AppError::Forbidden => HttpResponse::Forbidden().body("403 Forbidden"),
            AppError::Unauthorized => HttpResponse::Unauthorized().body("401 Unauthorized"),
            AppError::BadRequest(msg) => HttpResponse::BadRequest().body(msg.clone()),
            AppError::Database(e) => {
                log::error!("Database error: {}", e);
                HttpResponse::InternalServerError().body("Internal Server Error")
            }
            AppError::Internal(e) => {
                log::error!("Internal error: {}", e);
                HttpResponse::InternalServerError().body("Internal Server Error")
            }
        }
    }
}
