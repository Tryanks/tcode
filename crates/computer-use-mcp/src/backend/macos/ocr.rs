use objc2::AnyThread;
use objc2_foundation::{NSArray, NSData, NSDictionary};
use objc2_vision::{
    VNImageRequestHandler, VNRecognizeTextRequest, VNRequest, VNRequestTextRecognitionLevel,
};

use super::super::{BackendError, BackendErrorCode};
use crate::outline::{Frame, RecognizedTextBox};

pub(super) fn recognize_text(png: &[u8]) -> Result<Vec<RecognizedTextBox>, BackendError> {
    let image_data = NSData::with_bytes(png);
    let options = NSDictionary::new();
    let handler = VNImageRequestHandler::initWithData_options(
        VNImageRequestHandler::alloc(),
        &image_data,
        &options,
    );
    let request = VNRecognizeTextRequest::new();
    request.setRecognitionLevel(VNRequestTextRecognitionLevel::Accurate);
    request.setUsesLanguageCorrection(true);
    request.setAutomaticallyDetectsLanguage(true);
    let request_base: &VNRequest = request.as_ref();
    let requests = NSArray::<VNRequest>::from_slice(&[request_base]);
    handler
        .performRequests_error(&requests)
        .map_err(|error| vision_error(error.localizedDescription().to_string()))?;

    let Some(observations) = request.results() else {
        return Ok(Vec::new());
    };
    Ok(observations
        .to_vec()
        .into_iter()
        .filter_map(|observation| {
            let candidate = observation.topCandidates(1).to_vec().into_iter().next()?;
            // SAFETY: Vision produced this observation, and `boundingBox` is
            // a value-returning accessor whose CGRect ABI is bound by objc2.
            let bounding_box = unsafe { observation.boundingBox() };
            Some(RecognizedTextBox {
                text: candidate.string().to_string(),
                normalized_frame: Frame {
                    x: bounding_box.origin.x,
                    y: bounding_box.origin.y,
                    w: bounding_box.size.width,
                    h: bounding_box.size.height,
                },
            })
        })
        .collect())
}

fn vision_error(detail: String) -> BackendError {
    BackendError::new(
        BackendErrorCode::ObservationFailed,
        format!("Vision text recognition failed: {detail}"),
    )
}
