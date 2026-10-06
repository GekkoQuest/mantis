-- toy.helper: "Greet" says hello in local chat through the chat module's intent.
-- `host.intent` exists only at the automation tier; at the presentation tier the
-- mod only reports that greeting is off.
state.greetings = state.greetings or 0
state.refused = state.refused or 0

function on_greet(payload)
  if host.intent == nil then
    state.refused = state.refused + 1
    return
  end
  if host.intent("std.chat.say", "hello") then
    state.greetings = state.greetings + 1
  end
end

mantis.on("toy.helper.greet", "on_greet")

function on_tick(t)
  if host.intent ~= nil then
    host.set("status", "automation: " .. tostring(state.greetings) .. " greetings sent")
  else
    host.set("status", "presentation only: greeting is off here")
  end
  host.set("greetings", state.greetings)
  host.set("refused", state.refused)
end
