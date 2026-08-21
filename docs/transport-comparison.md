# Video transport in open source KVMs

How other open source KVM-over-IP projects send video, compared to this project.

## PiKVM

- Main mode: WebRTC with H.264 video.
- Captures video with a tool called uStreamer, then hands it to a WebRTC server called Janus, which sends it to the browser.
- WebRTC uses UDP and tries to connect directly (peer-to-peer), using STUN servers to work out the network setup.
- Fallback mode: MJPEG (a stream of JPEG images) over plain HTTP. Simpler, but higher lag than WebRTC.

## JetKVM

- WebRTC with H.264 video (H.265 on newer firmware).
- Written in Go, runs on Linux.
- Claims 30-60ms lag on the local network.
- For access from outside the home network, it uses WebRTC through JetKVM's own cloud relay.

## TinyPilot

- Does not use WebRTC.
- Streams MJPEG over plain HTTP.
- Higher lag than the WebRTC-based devices (roughly 90-230ms vs 30-60ms).

## NanoKVM

- Same general camp as PiKVM and JetKVM: WebRTC with H.264 for its better modes.

## Summary

The modern, low-lag open source KVMs (PiKVM, JetKVM, NanoKVM) use WebRTC with H.264 video. The simpler, cheaper ones (TinyPilot, and PiKVM's fallback mode) use plain MJPEG over HTTP, which is easier to build but noticeably laggier.

## This project

simple_kvm uses WebTransport (built on QUIC) instead of WebRTC. This is a newer, different approach:

- WebRTC has wider compatibility today and handles getting through NATs and firewalls well, using STUN/TURN servers.
- WebTransport is newer and can run into problems that WebRTC generally doesn't, for example a local proxy silently blocking the QUIC traffic, or issues with self-signed certificates.

Sources:
- https://docs.pikvm.org/webrtc_config/
- https://github.com/pikvm/pikvm/blob/master/docs/webrtc_config.md
- https://github.com/pikvm/ustreamer
- https://www.cnx-software.com/2025/03/21/jetkvm-a-69-kvm-over-ip-solution-with-open-source-software/
- https://computingforgeeks.com/best-ip-kvm-homelab/
- https://tinypilotkvm.com/blogs/insights/build-a-kvm-over-ip-under-100
