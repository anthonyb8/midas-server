use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Debug, Deserialize, Serialize)]
pub struct ApiResponse<T> {
    pub status: String,
    pub message: String,
    pub code: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<T>,
}

impl<T> ApiResponse<T> {
    pub fn new(status: &str, message: &str, code: StatusCode, data: Option<T>) -> Self {
        Self {
            status: status.to_string(),
            message: message.to_string(),
            code: code.as_u16(),
            data,
        }
    }
}

impl<T: Serialize> IntoResponse for ApiResponse<T> {
    fn into_response(self) -> Response {
        let status = StatusCode::from_u16(self.code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let body = axum::Json(json!({
            "status": self.status,
            "message": self.message,
            "code": self.code,
            "data": self.data,
        }));
        (status, body).into_response()
    }
}

// pub fn clean_output(&self) -> String {
//     format!(
//         "ApiResponse {{ message: {}, body: {:?}, status_code: {} }}",
//         self.message, self.body, self.status_code
//     )
// }
// }
// impl From<Error> for ApiResponse<()> {
//     fn from(error: Error) -> Self {
//         match error {
//             Error::SqlError(msg) => ApiResponse {
//                 status: String::from("failed"),
//                 message: msg,
//                 code: StatusCode::BAD_REQUEST.as_u16(),
//                 data: None,
//             },
//             Error::TracingError(msg) => ApiResponse {
//                 status: String::from("failed"),
//                 message: msg.to_string(),
//                 code: StatusCode::BAD_REQUEST.as_u16(),
//                 data: None,
//             },
//             Error::IoError(msg) => ApiResponse {
//                 status: String::from("failed"),
//                 message: msg.to_string(),
//                 code: StatusCode::BAD_REQUEST.as_u16(),
//                 data: None,
//             },
//             Error::EnvVarError(msg) => ApiResponse {
//                 status: String::from("failed"),
//                 message: msg.to_string(),
//                 code: StatusCode::BAD_REQUEST.as_u16(),
//                 data: None,
//             },
//             // Error::InternalServerError => ApiResponse {
//             //     message: "Internal server error".to_string(),
//             //     status_code: StatusCode::INTERNAL_SERVER_ERROR,
//             // },
//         }
//     }
// }