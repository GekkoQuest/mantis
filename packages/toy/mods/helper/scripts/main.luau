-- toy.helper: "Greet" says hello through the package's chat, if it has one (the
-- chat view model publishes `std.chat.enabled`). `host.intent` exists only at the
-- automation tier; at the presentation tier the mod only reports that greeting is off.
state.greetings = state.greetings or 0
state.refused = state.refused or 0

function has_chat()
  return host.get("std.chat.enabled") == true
end

function on_greet(payload)
  if host.intent == nil then
    state.refused = state.refused + 1
    return
  end
  if has_chat() and host.intent("std.chat.say", "hello") then
    state.greetings = state.greetings + 1
  end
end

mantis.on("toy.helper.greet", "on_greet")

function on_tick(t)
  if host.intent == nil then
    host.set("status", "presentation only: greeting is off here")
  elseif not has_chat() then
    host.set("status", "this package has no chat to greet in")
  else
    host.set("status", "automation: " .. tostring(state.greetings) .. " greetings sent")
  end
  host.set("greetings", state.greetings)
  host.set("refused", state.refused)
end
