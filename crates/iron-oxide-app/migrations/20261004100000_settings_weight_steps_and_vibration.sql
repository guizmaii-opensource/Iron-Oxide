-- The weight steppers' increment and vibration move from the device into the user's settings
-- (#103), so they follow the user to every device. One increment per unit, as before: 2.5 kg and
-- 5 lb (2267.96185 g, exact in nanograms). Vibration on. The defaults are what the devices used.
ALTER TABLE user_settings
    ADD COLUMN kg_weight_step_ng bigint NOT NULL DEFAULT 2500000000000
        CHECK (is_weight_ng(kg_weight_step_ng) AND kg_weight_step_ng > 0),
    ADD COLUMN lb_weight_step_ng bigint NOT NULL DEFAULT 2267961850000
        CHECK (is_weight_ng(lb_weight_step_ng) AND lb_weight_step_ng > 0),
    ADD COLUMN vibration_enabled boolean NOT NULL DEFAULT true;
