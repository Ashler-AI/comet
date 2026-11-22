-- A chat on one of Crew's sandbox hosts, on its Cloud tray alone
-- (screens/chat/cloud_tray): in a Crew build it follows its sandbox as a Cloud
-- chat follows its machine, so a sandbox that failed says so, and why, over
-- the composer; anywhere else it isn't one of Cloud's, and nothing is read.
-- src/crew_tests.rs mounts it with the host's `crew` flag and without.
--
-- The Cloud sessions are answered here (no engine in a test host), as the
-- engine answers a sandbox that failed to start (crates/engine/src/crew_cloud.rs
-- `failed_words`); how often they were read is `crew_cloud_tray_reads`.

local cloud = require("@account/cloud")

local HOST = "comet-scaffold-rcs_0123456789abcdef01234567-e1"
local CHAT = "9fa0da85-1612-407c-a020-0a8ba833f7c5"
local WHY = "The sandbox didn't start: missing session operation response. Start a new chat to try again."

local reads = ui.state("crew_cloud_tray_reads", 0)

-- A promise already settled, as the host's settle: `:after` at once.
local function settled(value: any): any
	local p = { done = true }
	function p.after(_self: any, ok: ((any) -> ())?, _bad: ((any) -> ())?)
		if ok then
			ok(value)
		end
	end
	return p
end

cloud.ask_sessions = function(): any
	reads:set(reads:peek() + 1)
	return settled { { chat = CHAT, device = HOST, space = "", state = "error", error = WHY } }
end
-- (Cloud's own status isn't what's shown here: never read.)
(cloud :: any).load = function() end

local CloudTray = require("@app/screens/chat/cloud_tray")
local machine = CloudTray.bind {
	chat = function(): string?
		return CHAT
	end,
	device = function(): string?
		return HOST
	end,
}

return ui.column {
	ui.text { id = "alert-title", literal = true, function(): string
		local a = machine.alert:get()
		return if a then a.title else "no alert"
	end },
	ui.text { id = "alert-body", literal = true, function(): string
		local a = machine.alert:get()
		return if a then a.body else "no alert"
	end },
}
