wasmtime::component::bindgen!({
    world: "vendor",
    path: "../stravia-vendor-sdk/wit",
    with: {
        "wasi": wasmtime_wasi::p2::bindings,
        "stravia:vendor/host.http-response": crate::host::HttpResponseResource,
        "stravia:vendor/host.ws-connection": crate::host::WebSocketResource,
    },
    imports: {
        "stravia:vendor/host@0.3.0": async | trappable,
    },
    exports: {
        default: async,
    },
});
