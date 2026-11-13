-- Crew's way through setup (screens/onboarding/crew), run inside a mounted
-- app as ui/onboarding_order.luau runs Zeron's: where a run opens, where
-- Continue and Back go and the dots, signed in with Scaffold, whatever the
-- account turns out to have. src/crew_tests.rs mounts it with the host's
-- `crew` flag on and reads #result ("ok" when nothing failed, else what
-- did, a line each); its `crew_test.signed_in` command signs in as setup
-- does, through the host's (Crew's) answers.

local state = require("@screens/onboarding/state")
local actions = require("@screens/onboarding/actions")
local crew = require("@account/crew")

local failures: { string } = {}
local current = ""

local function same(a: any, b: any): boolean
	if type(a) ~= "table" or type(b) ~= "table" then
		return a == b
	end
	if #a ~= #b then
		return false
	end
	for i = 1, #a do
		if a[i] ~= b[i] then
			return false
		end
	end
	return true
end

local function show(v: any): string
	if type(v) == "table" then
		return "{" .. table.concat(v, ", ") .. "}"
	end
	return tostring(v)
end

local function expect(actual: any)
	return {
		to_be = function(wanted: any)
			if not same(actual, wanted) then
				table.insert(failures, `{current}: got {show(actual)}, wanted {show(wanted)}`)
			end
		end,
	}
end

local function test(name: string, body: () -> ())
	current = name
	local ok, err = pcall(body)
	if not ok then
		table.insert(failures, `{name}: {err}`)
	end
end

type Run = { path: string?, name_done: boolean?, org_done: boolean?, org_admin: boolean?, passed: { string }? }

-- A run, as `signed_in` (or `skip_sign_in`) leaves it once the answers came.
local function run(r: Run): string
	state.path:set((r.path or "signed_in") :: any)
	state.name_done:set(r.name_done == true)
	state.org_done:set(r.org_done == true)
	state.org_admin:set(r.org_admin == true)
	state.claimed:set(false)
	state.device_done:set({})
	state.step:set("signin")
	actions.start_run(r.passed or {})
	return state.step:peek()
end

-- Every step Continue visits from `from` up to done.
local function forward(from: string): { string }
	state.step:set(from :: any)
	local seen = { from }
	for _ = 1, 12 do
		if state.step:peek() == "done" then
			break
		end
		actions.next()
		table.insert(seen, state.step:peek())
	end
	return seen
end

-- Every step Back visits from `from` until it stops.
local function backward(from: string): { string }
	state.step:set(from :: any)
	local seen = { from }
	for _ = 1, 12 do
		local before = state.step:peek()
		actions.back()
		if state.step:peek() == before then
			break
		end
		table.insert(seen, state.step:peek())
	end
	return seen
end

test("this is a Crew build", function()
	expect(crew.on).to_be(true)
end)

test("signed in: the look, the agents, done", function()
	expect(run {}).to_be("appearance")
	expect(forward("appearance")).to_be({ "appearance", "providers", "done" })
	expect(state.order()).to_be({ "signin", "appearance", "providers", "done" })
end)

test("back from done goes through every step, never to Sign in", function()
	run {}
	expect(backward("done")).to_be({ "done", "providers", "appearance" })
end)

test("no account step, whatever the account turns out to have", function()
	for _, r in { { name_done = true }, { org_done = true, org_admin = true }, { org_done = true, org_admin = false } } do
		expect(run(r :: Run)).to_be("appearance")
		expect(forward("appearance")).to_be({ "appearance", "providers", "done" })
	end
	-- (Even past them by hand: Continue goes on to the agents.)
	run {}
	expect(forward("username")).to_be({ "username", "providers", "done" })
end)

test("pages finished before are passed over at the start", function()
	expect(run { passed = { "appearance" } }).to_be("providers")
	expect(run { passed = { "appearance", "providers" } }).to_be("done")
	expect(backward("done")).to_be({ "done", "providers", "appearance" })
end)

test("the dots count three steps", function()
	run {}
	local n, count = state.position("appearance")
	expect({ n, count }).to_be({ 1, 3 })
	n, count = state.position("providers")
	expect({ n, count }).to_be({ 2, 3 })
	n = state.position("done")
	expect(n).to_be(3)
end)

test("someone welcomed back still goes Crew's way", function()
	state.welcome = true
	expect(run {}).to_be("appearance")
	expect(forward("appearance")).to_be({ "appearance", "providers", "done" })
	state.welcome = false
end)

test("without an account, as Zeron's: the look, the agents, done, back to Sign in", function()
	expect(run { path = "skipped" }).to_be("appearance")
	expect(forward("appearance")).to_be({ "appearance", "providers", "done" })
	expect(backward("done")).to_be({ "done", "providers", "appearance", "signin" })
end)

-- Signed in, as setup's Sign in does once the engine says so: it asks the
-- host for the pages finished, the username and the organizations (Crew's
-- answers), works in the project, and opens on the first page left.
ui.command {
	id = "crew_test.signed_in",
	title = "Signed in",
	run = function()
		state.device_done:set({})
		state.step:set("signin")
		actions.signed_in()
	end,
}

return ui.text {
	id = "result",
	if #failures == 0 then "ok" else table.concat(failures, "\n"),
}
