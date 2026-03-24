use std::sync::Arc;

use axum::extract::{Path, State};

use crate::{error::ApiError, state::Response};

use smelter_api::InputId;

use super::ApiState;

#[utoipa::path(
    post,
    path = "/api/input/{input_id}/subscribe",
    operation_id = "subscribe_input",
    params(("input_id" = str, Path, description = "Input ID.")),
    responses(
        (status = 200, description = "Subscribed to input successfully.", body = Response),
        (status = 400, description = "Bad request.", body = ApiError),
        (status = 404, description = "Input not found.", body = ApiError),
        (status = 500, description = "Internal server error.", body = ApiError),
    ),
    tags = ["subscribe_request"],
)]
pub async fn handle_subscribe_input(
    State(api): State<Arc<ApiState>>,
    Path(input_id): Path<InputId>,
) -> Result<Response, ApiError> {
    let api = api.clone();
    tokio::task::spawn_blocking(move || {
        api.pipeline()?
            .lock()
            .unwrap()
            .subscribe_input(&input_id.into())?;
        Ok(Response::Ok {})
    })
    .await
    .unwrap()
}
