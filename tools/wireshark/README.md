# ApertureC Wireshark Dissector


## Installation

1. Run the `install.sh` script. This will put the Lua dissector script in
   `$HOME/.local/lib/wireshark/plugins`.
2. Open the Wireshark GUI
3. Go to `Edit->Preferences` to open the Preferences window
4. On the left hand side, expand `Protocols` and select `ProtoBuf`
5. Check the following boxes:
  - `Load .proto files on startup.`
  - `Dissect Protobuf fields as Wireshark fields`
  - `Show details of message, fields and enums.`
6. Click `Edit` next to `Protobuf search paths` to pull up the Protobuf search
   paths window
  a. Click the plus sign on the bottom left hand corner to create a new entry
  b. Select `Browse` and navigate to the `protocol/proto` directory in the
  ApertureC repository root and select `Choose`
  c. Select the `Load all files` box next to this entry
  d. Repeat a-c above but add `/usr/local/include`
7. Select OK to exit out from all windows and return to Wireshark


## Usage

Open up any ApertureC packet capture to get started. You should be able to
filter based on channel type. For example, typing `ac_cc` in the filter window
will yield only Control Channel messages.

The channels are as follows:
- Control Channel: `ac_cc`
- Event Channel: `ac_ec`
- Media Channel: `ac_mmX` where X is a value 0-32

By default, the dissector assumes the control channel is port 46452, the event
channel is port 46453, and the media channels are the next subsequent 32
channels.

You may also filter on specific protobuf messages and fields. To filter on a
message, use `pbm`. For example, the following shows all ClientToServer messages
on the event channel: `pbm.event.ClientToServer`.

`pbf` works similarly. The following shows all heartbeat responses after 100:
`pbf.control.HeartbeatResponse.heartbeat_id > 100`.


## Requirements

This dissector was developed and tested on Wireshark 3.6.7, the default on
Ubuntu 18.04.
