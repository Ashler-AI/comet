-- Settings → Providers in a Crew build (ui/account/crew.luau): Crew's shared
-- accounts, the Agent Auth logins every agent it runs signs in with (a Crew
-- engine answers the account calls from Agent Auth: crates/engine
-- crew_accounts.rs). Its Claude accounts and its ChatGPT accounts, a card
-- each, as Crew's own app listed them (Anthropic's, then OpenAI's): each
-- account's plan, how it stands with Agent Auth and how much of its two
-- windows is used; adding one (its sign-in, in the browser); and removing
-- one, asked first, since Crew stops running agents on it. Nothing to put in
-- use: a chat runs on one automatically, or on the one its Account option
-- names.
--
-- The accounts come as the agents' do (account/agents): Claude's as Claude
-- Code's, ChatGPT's as Codex's. (OMP's are the same accounts again, by
-- provider: not shown twice.) A sign-in is that agent's, so the card shows
-- it under way as the agent's own card would.

local ds = require("@ds")
local agents = require("@account/agents")
local toast = require("@ds/toast")
local host = require("@app/screens/settings/host")
local search = require("@app/screens/settings/search")

local M = {}

-- The accounts' kinds, each with the agent whose sign-in adds one.
local KINDS = {
	{ key = "claude", agent = "claude-code", name = "Claude", mark = "claude" },
	{ key = "chatgpt", agent = "codex", name = "ChatGPT", mark = "openai" },
}

-- The names Agent Auth gives a family's two windows (crates/engine
-- crew_accounts.rs `usage_windows`), lowercased: its meters.
local SESSION = { ["5-hour"] = true, ["5h"] = true, session = true }
local WEEK = { weekly = true, week = true, ["7-day"] = true, ["7d"] = true }

-- The account being removed, once asked: its agent and its id.
local confirming = ui.state("crew_accounts_confirming", false)
local removing = ui.state("crew_accounts_removing", nil :: { agent: string, account: string }?)

local function remove()
	local which = removing:peek()
	if not which then
		return
	end
	-- (Refused or out of reach: said, never swallowed.)
	host.call("agents.remove", { id = which.agent, account = which.account }):after(function() end, function(err: any)
		local message = if type(err) == "table" then err.message else nil
		toast.show {
			title = ui.t("crew.accounts.couldnt_remove"):get(),
			body = if message == "offline" or message == nil
				then ui.t("settings.providers.cant_reach", { device = ui.t("settings.providers.this_mac"):get() }):get()
				else message,
			tone = "danger",
		}
	end)
end

-- One kind's card: its accounts as its agent's row has them, and that
-- agent's sign-in.
local function card(kind: { key: string, agent: string, name: string, mark: string }): Node
	local id = kind.agent
	-- (As it stands: read where it's drawn, or peeked at in a handler.)
	local function row(now: agents.Now?): agents.Agent?
		for _, a in (now or agents.now:get()).list do
			if a.id == id then
				return a
			end
		end
		return nil
	end
	local function field(name: string): any
		local a = row()
		return if a then (a :: any)[name] else nil
	end
	return ds.ProviderCard.Card {
		provider = { id = `crew-{kind.key}`, name = kind.name, mark = kind.mark },
		-- (No version, Update or switch: not an agent.)
		fixed = true,
		settings = false,
		enabled = ui.state(`crew_accounts_{kind.key}_on`, true),
		accounts = function(): { any }
			local a = row()
			local list: { agents.Account } = if a then a.accounts else {}
			local out = {}
			for _, acc in list do
				local one = table.clone(acc) :: any
				-- (Nothing to put in use: a chat picks its own.)
				one.active = false
				one.switchable = false
				-- Its meters, by the names Agent Auth gives its windows: the
				-- 5-hour one and the Weekly one (one it hasn't seen used: at
				-- 0). Any other ("Opus weekly", "5-hour (Spark)") is said in
				-- words.
				local windows: { any } = (acc :: any).windows or {}
				local session, week = nil, nil
				local others: { any } = {}
				for _, w in windows do
					local name = string.lower(w.label)
					if not session and SESSION[name] then
						session = w
					elseif not week and WEEK[name] then
						week = w
					else
						table.insert(others, w)
					end
				end
				one.session = if session then session.used else 0
				one.session_resets = if session then session.resets else nil
				one.week = if week then week.used else 0
				one.week_resets = if week then week.resets else nil
				-- After its plan: how it stands with Agent Auth when it can't
				-- be used ("Needs attention", "Disabled", "Revoked"), and its
				-- usage the meters don't show.
				local words: { string } = {}
				if acc.plan ~= "" then
					table.insert(words, acc.plan)
				end
				local why = (acc :: any).error
				if type(why) == "string" and why ~= "" then
					table.insert(words, why)
				end
				if #windows == 0 then
					table.insert(words, ui.t("crew.accounts.no_usage"):get())
				end
				for _, w in others do
					local n = math.floor(w.used * 100 + 0.5)
					table.insert(words, `{w.label} {ui.t("ds.composer.context.percent", { n = n }):get()}`)
				end
				one.plan = table.concat(words, " · ")
				table.insert(out, one)
			end
			return out
		end,
		active = function(): string?
			return nil
		end,
		foot = function(): string
			local a = row()
			return if a then a.login else "idle"
		end,
		on_add = function()
			agents.sign_in(id)
		end,
		on_code = function(code: string)
			agents.complete(id, code)
		end,
		settle = function(next: string)
			local a = row(agents.now:peek())
			if not a then
				return
			end
			if next == "waiting" then
				agents.retry(a)
			elseif a.login == "waiting" or a.login == "code" then
				agents.cancel(id)
			else
				agents.dismiss(id)
			end
		end,
		login_note = function(): string?
			return field("login_note")
		end,
		login_page = function(): boolean
			return field("login_page") ~= nil
		end,
		on_open_page = function()
			agents.open_page(id)
		end,
		login_code = function(): string?
			return field("login_code")
		end,
		on_remove = function(acc: any)
			if acc.id then
				removing:set({ agent = id, account = acc.id })
				confirming:set(true)
			end
		end,
	}
end

-- The section, on this computer's page (another's agents are its own, and
-- the shared accounts are only managed from here).
function M.section(): Node
	return ui.show(
		function(): boolean
			return agents.now:get().target == nil
		end,
		search.anchor(
			"shared_accounts",
			ui.column {
				w_full = true,
				ds.Settings.Section {
					title = ui.t("crew.accounts.title"),
					description = ui.t("crew.accounts.description"),
					ui.column {
						w_full = true,
						gap = "$space.4",
						card(KINDS[1]),
						card(KINDS[2]),
					},
				},
				ds.Dialog {
					open = confirming,
					title = ui.t("crew.accounts.remove_title"),
					body = ui.t("crew.accounts.remove_body"),
					tone = "danger",
					confirm = { label = ui.t("crew.accounts.remove"), on_click = remove },
				},
			}
		)
	)
end

return M
