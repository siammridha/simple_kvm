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

simple_kvm used WebTransport (built on QUIC) at first, then moved to WebRTC - joining PiKVM, JetKVM, and NanoKVM's approach. Video is a hybrid for comparison: MJPEG over a WebRTC data channel, H.264 over a real WebRTC video track.

The reason for the move was TLS, not performance. This device has no public hostname and no certificate from a public CA - just a private, self-signed one. WebTransport's normal certificate checking requires the certificate to be logged in **Certificate Transparency**, a set of public logs that only publicly trusted CAs can submit to; a private CA can never satisfy that, full stop, no matter how well the root is trusted on the device doing the connecting. The only workarounds were getting a certificate from a public CA for a domain this device doesn't have, or pinning the connection to the certificate's exact hash (`serverCertificateHashes`) and rotating it every 14 days.

WebRTC doesn't have this problem. Its encryption (DTLS-SRTP) is mandatory, automatic, and self-signed per connection - verified by a fingerprint exchanged during signaling, never checked against a certificate authority or a CT log. There's nothing for an operator to provide or rotate. See [webtransport-certificates.md in the git history](https://github.com/siammridha/simple_kvm/blob/6b14646/docs/webtransport-certificates.md) for the full investigation that found this.

Sources:
- https://docs.pikvm.org/webrtc_config/
- https://github.com/pikvm/pikvm/blob/master/docs/webrtc_config.md
- https://github.com/pikvm/ustreamer
- https://www.cnx-software.com/2025/03/21/jetkvm-a-69-kvm-over-ip-solution-with-open-source-software/
- https://computingforgeeks.com/best-ip-kvm-homelab/
- https://tinypilotkvm.com/blogs/insights/build-a-kvm-over-ip-under-100
- https://www.idmanagement.gov/implement/announcements/03_google_ct/
- https://chromestatus.com/feature/5690646332440576
