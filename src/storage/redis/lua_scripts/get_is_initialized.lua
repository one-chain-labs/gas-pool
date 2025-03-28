-- Copyright (c) Mysten Labs, Inc.
-- SPDX-License-Identifier: Apache-2.0

-- This script is used to check if the sponsor's gas pool has been initialized.
-- The first argument is the sponsor's address.

local sponsor_addresses = cjson.decode(ARGV[1])

for _, sponsor_address in ipairs(sponsor_addresses) do
    local initialized_key = sponsor_address .. ':initialized'
    if redis.call('EXISTS', initialized_key) == 0 then
        return 0
    end
end

return 1


