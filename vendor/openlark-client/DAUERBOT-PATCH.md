# Local patch

Upstream openlark-client 0.22.0 from crates.io (Apache-2.0).
The only behavioral addition is `LarkWsClient::open_with_ready`, whose oneshot
sender fires after the authenticated WebSocket upgrade completes.
Default `open` behavior, event handling and ACK ordering are unchanged.
Used to prove candidate transport readiness during a service handover.
