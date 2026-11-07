wasmtime::component::bindgen!({
    world: "vendor",
    path: "../stravia-vendor-sdk/wit",
    with: {
        "wasi": wasmtime_wasi::p2::bindings,
        "stravia:net/types.options": crate::host::NetOptionsResource,
        "stravia:net/http.response": crate::host::HttpResponseResource,
        "stravia:net/websocket.connection": crate::host::WebSocketResource,
    },
    imports: {
        "stravia:vendor/host@1.0.0": async | trappable,
        "stravia:net/types@1.0.0": async | trappable,
        "stravia:net/http@1.0.0": async | trappable,
        "stravia:net/websocket@1.0.0": async | trappable,
    },
    exports: {
        "execute": async | store,
        default: async,
    },
});
