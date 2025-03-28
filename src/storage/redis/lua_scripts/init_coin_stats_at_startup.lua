-- Copyright (c) Mysten Labs, Inc.
-- SPDX-License-Identifier: Apache-2.0

-- This script is used to initialize a few coin related statistics for a sponsor address at startup.
-- Including the total balance and the total coin count.
-- The first argument is the sponsor's address.
-- Returns a table with the new coin count and new total balance.

local sponsor_addresses = cjson.decode(ARGV[1])

local results = {}

for _, sponsor_address in ipairs(sponsor_addresses) do
    local t_available_gas_coins = sponsor_address .. ':available_gas_coins'

    local t_available_coin_count = sponsor_address .. ':available_coin_count'
    local coin_count = redis.call('GET', t_available_coin_count)
    if not coin_count then
        coin_count = redis.call('LLEN', t_available_gas_coins)
        redis.call('SET', t_available_coin_count, coin_count)
    end

    local t_available_coin_total_balance = sponsor_address .. ':available_coin_total_balance'
    local total_balance = redis.call('GET', t_available_coin_total_balance)
    if not total_balance then
        local elements = redis.call('LRANGE', t_available_gas_coins, 0, -1)
        total_balance = 0
        for _, coin in ipairs(elements) do
            -- Each coin is just a string, using "," to separate fields. The first is balance.
            local idx, _ = string.find(coin, ',', 1)
            local balance = string.sub(coin, 1, idx - 1)
            total_balance = total_balance + tonumber(balance)
        end
        redis.call('SET', t_available_coin_total_balance, total_balance)
    end

    table.insert(results, {
        sponsor_address,
        tonumber(coin_count, 10),
        tonumber(total_balance, 10)
    })

end

return cjson.encode(results)
