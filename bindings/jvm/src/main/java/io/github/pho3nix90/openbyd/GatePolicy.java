package io.github.pho3nix90.openbyd;

import io.github.pho3nix90.openbyd.model.CommandGateVerdict;

/**
 * Whether a command is sent, given the command gate's verdict: {@link #STRICT}, {@link
 * #ALLOW_MISSING_DATA}, or a function deciding from the verdict.
 *
 * <p>A function is asked on the client's callback thread. If it throws, the verdict stands and
 * the failure is reported to {@link ClientOptions#onCallbackError}.
 */
@FunctionalInterface
public interface GatePolicy {
    /** Only when the gate says the command is supported. The default, and pyBYD's rule. */
    GatePolicy STRICT = (vin, verdict) -> verdict.supported();

    /**
     * Also when the car's capability data is incomplete: the reasons {@code function_no_missing}
     * and {@code learn_info_missing}.
     */
    GatePolicy ALLOW_MISSING_DATA =
            (vin, verdict) ->
                    verdict.supported()
                            || "function_no_missing".equals(verdict.reason())
                            || "learn_info_missing".equals(verdict.reason());

    /**
     * Whether the command is sent.
     *
     * @param vin The vehicle identification number.
     * @param verdict What the gate found.
     */
    boolean allows(String vin, CommandGateVerdict verdict);
}
