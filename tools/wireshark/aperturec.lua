do
    local protobuf_dissector = Dissector.get("protobuf")
    if protobuf_dissector == nil then
        error("No ProtoBuf dissector installed")
    end

    local function create_udp_proto(name, desc, s2c_msgtype, c2s_msgtype, server_port)
        local proto = Proto(name, desc)

        proto.dissector = function(buffer, pinfo, tree)
            local subtree = tree:add(proto, buffer())
            if s2c_msgtype ~= nil and pinfo.src_port == server_port then
                pinfo.private["pb_msg_type"] = "message," .. s2c_msgtype
            elseif c2s_msgtype ~= nil and pinfo.dst_port == server_port then
                pinfo.private["pb_msg_type"] = "message," .. c2s_msgtype
            end
            pcall(Dissector.call, protobuf_dissector, buffer, pinfo, subtree)
            pinfo.columns.protocol = proto.name
        end
        return proto
    end

    local function create_tcp_proto(name, desc, s2c_msgtype, c2s_msgtype, server_port)
        local function dissect_leb128(buffer, start_offset)
            local offset, shift, byte = start_offset, 0, 0
            local value = 0

            repeat
                if offset > buffer:len() then
                    error("Exceeded buffer")
                end

                byte = buffer(offset, 1):uint()
                offset = offset + 1

                value = bit.bor(value, bit.lshift(bit.band(byte, 0x7F), shift))
                shift = shift + 7
            until bit.band(byte, 0x80) == 0

            local len = offset - start_offset
            return value, len
        end

        local proto = Proto(name, desc)
        local msg_len_field = ProtoField.uint32(name .. ".message_length", "Message Length", base.DEC)
        proto.fields = {msg_len_field}

        proto.dissector = function(tvb, pinfo, tree)
            local t = tree:add(proto, tvb())
            local offset = 0

            while (tvb:len() - offset) > 0 do
                local disect_delim_res, msg_len, delim_len = pcall(dissect_leb128, tvb, offset)

                if disect_delim_res then
                    if (tvb:len() - (offset + delim_len)) < msg_len then
                        pinfo.desegment_offset = offset
                        pinfo.desegment_len = DESEGMENT_ONE_MORE_SEGMENT
                        print("not enough data for message")
                        return
                    end

                    if s2c_msgtype ~= nil and pinfo.src_port == server_port then
                        pinfo.private["pb_msg_type"] = "message," .. s2c_msgtype
                    elseif c2s_msgtype ~= nil and pinfo.dst_port == server_port then
                        pinfo.private["pb_msg_type"] = "message," .. c2s_msgtype
                    end

                    t:add(msg_len_field, tvb(offset, delim_len), msg_len)
                    offset = offset + delim_len

                    local status, err = pcall(Dissector.call, protobuf_dissector, tvb(offset, msg_len):tvb(), pinfo, t)
                    if not status then
                        print("Failed to dissect proto: " .. err)
                    end
                    pinfo.columns.protocol = proto.name
                    offset = offset + msg_len
                else
                    print("not enough data for delimiter")
                    pinfo.desegment_offset = offset
                    pinfo.desegment_len = DESEGMENT_ONE_MORE_SEGMENT
                    return
                end
            end
        end
        return proto
    end

    local cc_proto =
        create_tcp_proto(
        "ac_cc",
        "ApertureC Control Channel Message",
        "control.ServerToClient",
        "control.ClientToServer",
        46452
    )
    DissectorTable.get("tcp.port"):add(46452, cc_proto)

    local ec_proto =
        create_tcp_proto(
        "ac_ec",
        "ApertureC Event Channel Message",
        "event.ServerToClient",
        "event.ClientToServer",
        46453
    )
    DissectorTable.get("tcp.port"):add(46453, ec_proto)

    for idx = 0, 32, 1 do
        local port = idx + 46454
        local proto =
            create_udp_proto(
            "ac_mm" .. idx,
            "ApertureC Media Channel Message (" .. idx .. ")",
            "media.ServerToClient",
            "media.ClientToServer",
            port
        )
        DissectorTable.get("udp.port"):add(port, proto)
    end
end

