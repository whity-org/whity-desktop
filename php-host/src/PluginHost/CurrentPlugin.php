<?php

declare(strict_types=1);

namespace Whity\PluginHost;

/**
 * Which plugin's code is running right now. The host sets it around every
 * plugin route handler and hook callback, so a host service can bind what
 * it hands out to the caller (the native bridge client, first) without the
 * plugin naming itself. Outside plugin code it is null.
 *
 * Honest limit: this is a convention inside one PHP process. A plugin that
 * calls another plugin's code directly runs under the caller's name. Signed
 * plugins only, as the ADR says, is what makes that acceptable.
 */
final class CurrentPlugin
{
    /** @var list<string> */
    private static array $stack = [];

    public static function name(): ?string
    {
        return self::$stack === [] ? null : self::$stack[array_key_last(self::$stack)];
    }

    /**
     * Run `$fn` as `$pluginName`, restoring the previous plugin afterwards.
     *
     * @template T
     * @param callable(): T $fn
     * @return T
     */
    public static function run(string $pluginName, callable $fn): mixed
    {
        self::$stack[] = $pluginName;
        try {
            return $fn();
        } finally {
            array_pop(self::$stack);
        }
    }
}
