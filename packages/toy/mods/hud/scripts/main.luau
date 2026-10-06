-- toy.hud: shows the session's character and how many chat lines arrived, and
-- plays a click when the ping button is pressed. `host.*` exists only while the
-- mod runs (in handlers and on_tick), never at load.
state.pings = state.pings or 0

function on_ping(payload)
  state.pings = state.pings + 1
  host.play("ui.click")
end

mantis.on("toy.hud.ping", "on_ping")

function on_tick(t)
  local character = host.get("client.character")
  if character then
    host.set("character", "character " .. character)
  else
    host.set("character", "not in the world yet")
  end
  host.set("lines", "chat lines: " .. tostring(host.count("std.chat.lines")))
  host.set("pings", state.pings)
end
