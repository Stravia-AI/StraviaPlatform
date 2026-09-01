use super::*;
use crate::protocol::ir::EmbeddingVector;

#[test]
fn request_round_trip_uses_typed_embedding_fields() {
    let input = serde_json::json!({
        "model": "text-embedding-3-small",
        "input": ["first", "second"],
        "dimensions": 256,
        "encoding_format": "float",
        "user": "local-user"
    });

    let request = EmbeddingsDecoder.decode_request(input.clone()).unwrap();
    let (encoded, _) = EmbeddingsEncoder.encode_request(&request).unwrap();

    assert!(matches!(
        request.embedding.as_ref().map(|embedding| &embedding.input),
        Some(EmbeddingInput::Texts(values)) if values == &["first", "second"]
    ));
    assert_eq!(encoded, input);
}

#[test]
fn response_round_trip_uses_typed_vectors() {
    let input = serde_json::json!({
        "object": "list",
        "data": [{
            "object": "embedding",
            "index": 0,
            "embedding": [0.25, -0.5]
        }],
        "model": "text-embedding-3-small",
        "usage": {"prompt_tokens": 4, "total_tokens": 4}
    });

    let response = EmbeddingsResponseParser
        .parse_response(input.clone())
        .unwrap();
    let encoded = EmbeddingsResponseFormatter.format_response(&response);

    assert!(matches!(
        response
            .embedding_output
            .as_ref()
            .and_then(|output| output.data.first())
            .map(|item| &item.embedding),
        Some(EmbeddingVector::Floats(values)) if values == &[0.25, -0.5]
    ));
    assert_eq!(encoded, input);
}
