// SPDX-License-Identifier: GPL-2.0-or-later

#include <residfp/residfp.h>

#include "State.h"

#include <algorithm>
#include <cstdint>
#include <cstring>
#include <filesystem>
#include <fstream>
#include <iostream>
#include <limits>
#include <map>
#include <set>
#include <stdexcept>
#include <string>
#include <utility>
#include <vector>

#ifndef ORACLE_SOURCE_VERSION
#error "ORACLE_SOURCE_VERSION must be defined"
#endif
#ifndef ORACLE_SOURCE_REVISION
#error "ORACLE_SOURCE_REVISION must be defined"
#endif
#ifndef ORACLE_SOURCE_SHA256
#error "ORACLE_SOURCE_SHA256 must be defined"
#endif
#ifndef ORACLE_SOURCE_REPOSITORY
#error "ORACLE_SOURCE_REPOSITORY must be defined"
#endif
#ifndef ORACLE_SOURCE_TAG
#error "ORACLE_SOURCE_TAG must be defined"
#endif
#ifndef ORACLE_GENERATOR_REVISION
#error "ORACLE_GENERATOR_REVISION must be defined"
#endif
#ifndef ORACLE_BUILD_FLAGS
#error "ORACLE_BUILD_FLAGS must be defined"
#endif
#ifndef ORACLE_COMPILER_ID
#error "ORACLE_COMPILER_ID must be defined"
#endif

namespace {

using JsonFields = std::map<std::string, std::string>;

struct Operation {
    enum class Kind { Write, Observe };

    Kind kind;
    std::uint64_t sequence;
    std::uint64_t cycle;
    unsigned int reg = 0;
    unsigned int value = 0;
    std::string observation;
};

struct Observation {
    std::string id;
    std::uint64_t cycle;
    JsonFields values;
};

struct Case {
    std::string id;
    std::string description;
    reSIDfp::ChipModel model;
    std::vector<Operation> operations;
    std::vector<Observation> observations;
};

constexpr unsigned int V1_FREQ_LO = 0x00;
constexpr unsigned int V1_FREQ_HI = 0x01;
constexpr unsigned int V1_PW_LO = 0x02;
constexpr unsigned int V1_PW_HI = 0x03;
constexpr unsigned int V1_CTRL = 0x04;
constexpr unsigned int V1_AD = 0x05;
constexpr unsigned int V1_SR = 0x06;
constexpr unsigned int V2_FREQ_LO = 0x07;
constexpr unsigned int V2_FREQ_HI = 0x08;
constexpr unsigned int V2_CTRL = 0x0b;
constexpr unsigned int V3_FREQ_LO = 0x0e;
constexpr unsigned int V3_FREQ_HI = 0x0f;
constexpr unsigned int V3_PW_LO = 0x10;
constexpr unsigned int V3_PW_HI = 0x11;
constexpr unsigned int V3_CTRL = 0x12;
constexpr unsigned int V3_AD = 0x13;
constexpr unsigned int V3_SR = 0x14;

std::string quote(const std::string& input) {
    std::string output = "\"";
    for (const char ch : input) {
        switch (ch) {
        case '\\': output += "\\\\"; break;
        case '"': output += "\\\""; break;
        case '\n': output += "\\n"; break;
        case '\r': output += "\\r"; break;
        case '\t': output += "\\t"; break;
        default: output += ch; break;
        }
    }
    output += '"';
    return output;
}

const char* model_name(const reSIDfp::ChipModel model) {
    return model == reSIDfp::MOS6581 ? "mos6581" : "mos8580";
}

const char* phase_name(const reSIDfp::EnvelopeGenerator::State state) {
    switch (state) {
    case reSIDfp::EnvelopeGenerator::State::ATTACK: return "attack";
    case reSIDfp::EnvelopeGenerator::State::DECAY_SUSTAIN: return "decay_sustain";
    case reSIDfp::EnvelopeGenerator::State::RELEASE: return "release";
    }
    throw std::runtime_error("unknown libresidfp envelope state");
}

void configure_sid(reSIDfp::residfp& sid, const reSIDfp::ChipModel model) {
    if (!sid.setChipModel(model)) throw std::runtime_error("libresidfp rejected chip model");
    if (!sid.setCombinedWaveforms(reSIDfp::AVERAGE)) {
        throw std::runtime_error("libresidfp rejected combined waveform strength");
    }
    if (!sid.setSamplingParameters(985248.0, reSIDfp::DECIMATE, 44100.0)) {
        throw std::runtime_error("libresidfp rejected diagnostic save-state sampling setup");
    }
    sid.reset();
}

reSIDfp::State state_of(const reSIDfp::residfp& sid) {
    const int size = sid.stateSize();
    if (size < static_cast<int>(sizeof(reSIDfp::State))) {
        throw std::runtime_error("libresidfp save state is smaller than State");
    }
    std::vector<char> buffer(static_cast<std::size_t>(size));
    if (sid.saveState(buffer.data(), size) < static_cast<int>(sizeof(reSIDfp::State))) {
        throw std::runtime_error("libresidfp failed to save State");
    }
    reSIDfp::State state{};
    std::memcpy(&state, buffer.data(), sizeof(state));
    return state;
}

std::uint16_t clock_rate_lfsr(const std::uint16_t value) {
    const unsigned int feedback = ((value << 14) ^ (value << 13)) & 0x4000;
    return static_cast<std::uint16_t>((value >> 1) | feedback);
}

unsigned int normalized_rate_counter(const reSIDfp::State& state, const std::size_t voice) {
    if (state.resetLfsr[voice]) return 0;
    std::uint16_t current = 0x7fff;
    for (unsigned int distance = 0; distance < 0x7fff; ++distance) {
        if (current == state.lfsr[voice]) return distance;
        current = clock_rate_lfsr(current);
    }
    throw std::runtime_error("invalid envelope rate LFSR state");
}

unsigned int normalized_rate_period(const std::uint16_t rate) {
    constexpr std::uint16_t comparisons[16]{
        0x007f, 0x3000, 0x1e00, 0x0660, 0x0182, 0x5573, 0x000e, 0x3805,
        0x2424, 0x2220, 0x090c, 0x0ecd, 0x010e, 0x23f7, 0x5237, 0x64a8
    };
    constexpr unsigned int periods[16]{
        9, 32, 63, 95, 149, 220, 267, 313,
        392, 977, 1954, 3126, 3907, 11720, 19532, 31251
    };
    for (std::size_t index = 0; index < 16; ++index) {
        if (comparisons[index] == rate) return periods[index];
    }
    throw std::runtime_error("unknown envelope rate comparison value");
}

std::uint32_t clock_noise_lfsr(const std::uint32_t value) {
    const std::uint32_t feedback = ((value ^ (value >> 5)) & 1) << 22;
    return (value >> 1) | feedback;
}

unsigned int normalized_noise_clock_count(const reSIDfp::State& state, const std::size_t voice) {
    std::uint32_t current = 0x3fffff;
    for (unsigned int count = 0; count <= 0x7fffff; ++count) {
        if (current == state.shift_register[voice]) {
            return count + (state.shift_pipeline[voice] != 0 ? 1 : 0);
        }
        current = clock_noise_lfsr(current);
    }
    throw std::runtime_error("noise shift register is outside the pure LFSR sequence");
}

std::string field_value(
    const reSIDfp::residfp& sid,
    const reSIDfp::State& state,
    const std::string& field
) {
    if (field == "public.env3") return std::to_string(sid.peek(0x1c));
    if (field == "public.osc3") return std::to_string(sid.peek(0x1b));
    for (std::size_t voice = 0; voice < 3; ++voice) {
        const std::string prefix = "voices." + std::to_string(voice + 1) + ".";
        if (field == prefix + "accumulator") return std::to_string(state.accumulator[voice]);
        if (field == prefix + "shift_register") return std::to_string(state.shift_register[voice]);
        if (field == prefix + "noise_clock_count") {
            return std::to_string(normalized_noise_clock_count(state, voice));
        }
        if (field == prefix + "envelope.level") return std::to_string(state.envelope_counter[voice]);
        if (field == prefix + "envelope.phase") return quote(phase_name(state.env_state[voice]));
        if (field == prefix + "envelope.gate") return state.gate[voice] ? "true" : "false";
        if (field == prefix + "envelope.rate_counter") {
            return std::to_string(normalized_rate_counter(state, voice));
        }
        if (field == prefix + "envelope.rate_period") {
            return std::to_string(normalized_rate_period(state.rate[voice]));
        }
        if (field == prefix + "envelope.exponential_counter") {
            return std::to_string(state.exponential_counter[voice]);
        }
        if (field == prefix + "envelope.exponential_period") {
            return std::to_string(state.exponential_counter_period[voice]);
        }
        if (field == prefix + "envelope.hold_zero") {
            return state.counter_enabled[voice] ? "false" : "true";
        }
        if (field == prefix + "test_fill_at") {
            return std::to_string(state.shift_register_reset[voice]);
        }
        if (field == prefix + "noise_poisoned") {
            const unsigned int waveform = state.waveform[voice];
            return (waveform & 0x8) != 0 && (waveform & 0x7) != 0 ? "true" : "false";
        }
    }
    throw std::runtime_error("unknown oracle field " + field);
}

JsonFields observe_fields(
    const reSIDfp::residfp& sid,
    const std::vector<std::string>& selected_fields
) {
    const reSIDfp::State state = state_of(sid);
    JsonFields fields;
    for (const std::string& field : selected_fields) {
        if (!fields.emplace(field, field_value(sid, state, field)).second) {
            throw std::runtime_error("duplicate selected oracle field " + field);
        }
    }
    return fields;
}

class CaseBuilder {
private:
    reSIDfp::residfp sid;
    Case test_case;
    std::uint64_t cycle = 0;
    std::uint64_t sequence = 0;

    void clock_to(const std::uint64_t target) {
        if (target < cycle) {
            throw std::runtime_error(
                "oracle case " + test_case.id + " clock moved backwards from "
                + std::to_string(cycle) + " to " + std::to_string(target)
            );
        }
        std::uint64_t remaining = target - cycle;
        while (remaining > 0) {
            const auto chunk = static_cast<unsigned int>(std::min<std::uint64_t>(
                remaining, std::numeric_limits<unsigned int>::max()
            ));
            sid.clockDigital(chunk);
            remaining -= chunk;
        }
        cycle = target;
    }

public:
    CaseBuilder(std::string id, std::string description, const reSIDfp::ChipModel model) {
        test_case.id = std::move(id);
        test_case.description = std::move(description);
        test_case.model = model;
        configure_sid(sid, model);
    }

    void write(const std::uint64_t at, const unsigned int reg, const unsigned int value) {
        clock_to(at);
        sid.write(static_cast<int>(reg), static_cast<unsigned char>(value));
        test_case.operations.push_back(Operation{
            Operation::Kind::Write, sequence++, at, reg, value, ""
        });
    }

    void observe(
        const std::uint64_t at,
        std::string id,
        const std::vector<std::string>& fields
    ) {
        clock_to(at);
        test_case.operations.push_back(Operation{
            Operation::Kind::Observe, sequence++, at, 0, 0, id
        });
        test_case.observations.push_back(Observation{
            std::move(id), at, observe_fields(sid, fields)
        });
    }

    template<typename Predicate>
    std::uint64_t advance_until(Predicate predicate, const std::uint64_t limit) {
        while (cycle < limit) {
            clock_to(cycle + 1);
            if (predicate(state_of(sid))) return cycle;
        }
        throw std::runtime_error("oracle condition was not reached in " + test_case.id);
    }

    const reSIDfp::State state() const { return state_of(sid); }
    std::uint64_t current_cycle() const { return cycle; }
    Case finish() { return std::move(test_case); }
};

Case reset_case(const reSIDfp::ChipModel model) {
    CaseBuilder builder(
        std::string("reset_") + model_name(model),
        "libresidfp state immediately after model selection and reset.",
        model
    );
    const std::vector<std::string> fields{
        "public.env3", "public.osc3",
        "voices.1.accumulator", "voices.1.shift_register", "voices.1.envelope.level",
        "voices.1.envelope.phase", "voices.1.envelope.gate",
        "voices.2.accumulator", "voices.2.shift_register", "voices.2.envelope.level",
        "voices.2.envelope.phase", "voices.2.envelope.gate",
        "voices.3.accumulator", "voices.3.shift_register", "voices.3.envelope.level",
        "voices.3.envelope.phase", "voices.3.envelope.gate"
    };
    builder.observe(0, "reset", fields);
    return builder.finish();
}

struct EnvelopeSchedule {
    std::uint64_t zero;
    std::uint64_t gate;
    std::uint64_t first_step;
};

EnvelopeSchedule envelope_schedule(const unsigned int attack) {
    reSIDfp::residfp sid;
    configure_sid(sid, reSIDfp::CSG8580);
    std::uint64_t cycle = 0;
    while (cycle < 100000) {
        sid.clockDigital(1);
        ++cycle;
        const reSIDfp::State state = state_of(sid);
        if (state.envelope_counter[2] == 0 && !state.counter_enabled[2]) break;
    }
    if (cycle == 100000) throw std::runtime_error("envelope did not settle to zero");
    const std::uint64_t gate = cycle + 4;
    sid.clockDigital(4);
    cycle = gate;
    sid.write(V3_AD, static_cast<unsigned char>(attack << 4));
    sid.write(V3_SR, 0xf0);
    sid.write(V3_CTRL, 0x01);
    const std::uint64_t limit = gate + 100000;
    while (cycle < limit) {
        sid.clockDigital(1);
        ++cycle;
        if (state_of(sid).envelope_counter[2] >= 2) {
            return EnvelopeSchedule{gate - 4, gate, cycle};
        }
    }
    throw std::runtime_error("envelope attack did not start");
}

Case envelope_attack_rate_case(const unsigned int attack) {
    const EnvelopeSchedule schedule = envelope_schedule(attack);
    const std::string suffix = attack < 10 ? "0" + std::to_string(attack) : std::to_string(attack);
    CaseBuilder builder(
        "envelope_attack_rate_" + suffix,
        "Attack rate " + std::to_string(attack) + " at the first envelope step boundary.",
        reSIDfp::CSG8580
    );
    builder.write(schedule.gate, V3_AD, attack << 4);
    builder.write(schedule.gate, V3_SR, 0xf0);
    builder.write(schedule.gate, V3_CTRL, 0x01);
    const std::vector<std::string> fields{
        "public.env3", "voices.3.envelope.level", "voices.3.envelope.phase",
        "voices.3.envelope.gate"
    };
    builder.observe(schedule.first_step - 1, "before_first_step", fields);
    builder.observe(schedule.first_step, "first_step", fields);
    builder.observe(schedule.first_step + 1, "after_first_step", fields);
    return builder.finish();
}

Case envelope_transitions_case() {
    const EnvelopeSchedule schedule = envelope_schedule(0);
    CaseBuilder builder(
        "envelope_transitions",
        "Attack, decay, release, retrigger, rate rewrite, and hold-zero boundaries.",
        reSIDfp::CSG8580
    );
    builder.write(schedule.gate, V3_AD, 0x00);
    builder.write(schedule.gate, V3_SR, 0x00);
    builder.write(schedule.gate, V3_CTRL, 0x01);
    const std::vector<std::string> fields{
        "public.env3", "voices.3.envelope.level", "voices.3.envelope.phase",
        "voices.3.envelope.rate_counter", "voices.3.envelope.rate_period",
        "voices.3.envelope.exponential_counter", "voices.3.envelope.exponential_period",
        "voices.3.envelope.gate", "voices.3.envelope.hold_zero"
    };
    builder.observe(schedule.gate, "gate_on", fields);
    const auto at_fe = builder.advance_until(
        [](const reSIDfp::State& state) { return state.envelope_counter[2] == 0xfe; },
        schedule.gate + 10000
    );
    builder.observe(at_fe, "attack_fe", fields);
    const auto at_ff = builder.advance_until(
        [](const reSIDfp::State& state) { return state.envelope_counter[2] == 0xff; },
        at_fe + 100
    );
    builder.observe(at_ff, "attack_ff", fields);
    builder.observe(at_ff + 4, "entered_decay", fields);

    const std::vector<unsigned int> thresholds{0x5d, 0x36, 0x1a, 0x0e, 0x06};
    for (const unsigned int threshold : thresholds) {
        const auto at = builder.advance_until(
            [threshold](const reSIDfp::State& state) {
                return state.envelope_counter[2] == threshold;
            },
            builder.current_cycle() + 200000
        );
        builder.observe(at, "decay_" + std::to_string(threshold), fields);
    }

    const auto release_at = builder.current_cycle() + 1;
    builder.write(release_at, V3_CTRL, 0x00);
    builder.observe(release_at, "gate_off", fields);
    const auto release_mid = builder.advance_until(
        [](const reSIDfp::State& state) { return state.envelope_counter[2] == 3; },
        release_at + 100000
    );
    builder.observe(release_mid, "release_level_3", fields);
    builder.write(release_mid, V3_CTRL, 0x01);
    builder.observe(release_mid, "retrigger_from_release", fields);
    builder.observe(release_mid + 20, "retrigger_progress", fields);
    builder.write(release_mid + 20, V3_CTRL, 0x00);
    const auto at_zero = builder.advance_until(
        [](const reSIDfp::State& state) {
            return state.envelope_counter[2] == 0 && !state.counter_enabled[2];
        },
        release_mid + 100000
    );
    builder.observe(at_zero, "release_zero", fields);
    builder.write(at_zero, V3_SR, 0xf0);
    builder.observe(at_zero + 64, "sustain_raised_while_zero", fields);
    builder.write(at_zero + 64, V3_CTRL, 0x01);
    builder.observe(at_zero + 64, "hold_zero_unlock", fields);
    builder.write(at_zero + 65, V3_AD, 0xf0);
    builder.observe(at_zero + 65, "rate_rewrite_after_gate", fields);
    builder.write(at_zero + 66, V3_SR, 0x07);
    builder.write(at_zero + 66, V3_CTRL, 0x00);
    builder.observe(at_zero + 66, "same_cycle_sr_gate_off", fields);
    return builder.finish();
}

Case envelope_attack_to_release_case() {
    const EnvelopeSchedule schedule = envelope_schedule(0);
    CaseBuilder builder(
        "envelope_attack_to_release",
        "Gate-off interrupts Attack directly, enters Release, and reaches zero.",
        reSIDfp::CSG8580
    );
    builder.write(schedule.gate, V3_AD, 0x00);
    builder.write(schedule.gate, V3_SR, 0x00);
    builder.write(schedule.gate, V3_CTRL, 0x01);
    const std::vector<std::string> fields{
        "public.env3", "voices.3.envelope.level", "voices.3.envelope.phase",
        "voices.3.envelope.rate_counter", "voices.3.envelope.rate_period",
        "voices.3.envelope.exponential_counter", "voices.3.envelope.exponential_period",
        "voices.3.envelope.gate", "voices.3.envelope.hold_zero"
    };
    const auto interrupt = builder.advance_until(
        [](const reSIDfp::State& state) {
            return state.env_state[2] == reSIDfp::EnvelopeGenerator::State::ATTACK
                && state.envelope_counter[2] == 0x20;
        },
        schedule.gate + 10000
    );
    builder.observe(interrupt, "attack_before_gate_off", fields);
    builder.write(interrupt, V3_CTRL, 0x00);
    builder.observe(interrupt, "gate_off_during_attack", fields);
    const auto release_started = builder.advance_until(
        [](const reSIDfp::State& state) {
            return state.env_state[2] == reSIDfp::EnvelopeGenerator::State::RELEASE;
        },
        interrupt + 10
    );
    builder.observe(release_started, "release_started", fields);
    const auto release_zero = builder.advance_until(
        [](const reSIDfp::State& state) {
            return state.envelope_counter[2] == 0 && !state.counter_enabled[2];
        },
        release_started + 100000
    );
    builder.observe(release_zero, "release_zero", fields);
    return builder.finish();
}

Case envelope_adsr_delay_case() {
    const EnvelopeSchedule schedule = envelope_schedule(15);
    CaseBuilder builder(
        "envelope_adsr_delay_wrap",
        "A rate rewrite below the current counter waits for the 15-bit ADSR-delay wrap.",
        reSIDfp::CSG8580
    );
    const std::vector<std::string> fields{
        "public.env3", "voices.3.envelope.level", "voices.3.envelope.phase",
        "voices.3.envelope.rate_counter", "voices.3.envelope.rate_period"
    };
    builder.write(schedule.gate, V3_AD, 0xf0);
    builder.write(schedule.gate, V3_SR, 0xf0);
    builder.write(schedule.gate, V3_CTRL, 0x01);
    const auto rewrite = builder.advance_until(
        [](const reSIDfp::State& state) {
            return state.envelope_counter[2] == 1
                && normalized_rate_counter(state, 2) > 100;
        },
        schedule.gate + 1000
    );
    const auto counter = normalized_rate_counter(builder.state(), 2);
    builder.observe(rewrite, "before_rate_rewrite", fields);
    builder.write(rewrite, V3_AD, 0x00);
    builder.observe(rewrite, "after_rate_rewrite", fields);
    const std::uint64_t expected_match = rewrite + (0x7fff - counter) + 9;
    builder.observe(expected_match - 1, "before_wrapped_match", fields);
    builder.observe(expected_match, "wrapped_match", fields);
    builder.observe(expected_match + 1, "after_wrapped_match", fields);
    return builder.finish();
}

Case envelope_same_cycle_order_case(const bool gate_first) {
    const EnvelopeSchedule schedule = envelope_schedule(3);
    CaseBuilder builder(
        gate_first ? "envelope_same_cycle_gate_ad_sr" : "envelope_same_cycle_ad_sr_gate",
        gate_first
            ? "Same-cycle gate, AD, and SR writes with gate first."
            : "Same-cycle AD, SR, and gate writes with gate last.",
        reSIDfp::CSG8580
    );
    if (gate_first) {
        builder.write(schedule.gate, V3_CTRL, 0x01);
        builder.write(schedule.gate, V3_AD, 0x34);
        builder.write(schedule.gate, V3_SR, 0xa5);
    } else {
        builder.write(schedule.gate, V3_AD, 0x34);
        builder.write(schedule.gate, V3_SR, 0xa5);
        builder.write(schedule.gate, V3_CTRL, 0x01);
    }
    const std::vector<std::string> fields{
        "public.env3", "voices.3.envelope.level", "voices.3.envelope.phase",
        "voices.3.envelope.rate_counter", "voices.3.envelope.rate_period",
        "voices.3.envelope.gate"
    };
    builder.observe(schedule.gate, "after_same_cycle_writes", fields);
    builder.observe(schedule.gate + 1, "one_cycle_after_writes", fields);
    builder.observe(schedule.first_step, "first_rate_step", fields);
    return builder.finish();
}

Case oscillator_trajectory_case() {
    CaseBuilder builder(
        "oscillator_frequency_and_saw",
        "Accumulator reset, frequency boundaries, 24-bit wrap, and saw OSC3 trajectory.",
        reSIDfp::CSG8580
    );
    const std::vector<std::string> fields{
        "public.osc3", "voices.3.accumulator", "voices.3.shift_register"
    };
    builder.write(0, V3_FREQ_LO, 0xff);
    builder.write(0, V3_FREQ_HI, 0xff);
    builder.write(0, V3_CTRL, 0x28);
    builder.observe(0, "test_set", fields);
    builder.write(1, V3_CTRL, 0x20);
    builder.observe(1, "test_clear", fields);
    builder.observe(2, "first_increment", fields);
    builder.observe(17, "saw_16_cycles", fields);
    builder.observe(130, "saw_msb_crossing", fields);
    builder.write(130, V3_FREQ_LO, 0x01);
    builder.observe(131, "frequency_low_rewrite", fields);
    builder.write(131, V3_FREQ_HI, 0x00);
    builder.observe(132, "frequency_high_rewrite", fields);
    builder.observe(20000000, "large_delta_wrap", fields);
    builder.write(20000000, V3_FREQ_LO, 0x00);
    builder.observe(20000010, "zero_frequency", fields);
    return builder.finish();
}

Case oscillator_waveforms_case() {
    CaseBuilder builder(
        "oscillator_waveforms",
        "Waveform zero, triangle halves, saw, pulse boundaries, ring modulation, and noise.",
        reSIDfp::CSG8580
    );
    const std::vector<std::string> fields{"public.osc3", "voices.3.accumulator"};
    builder.write(0, V2_FREQ_LO, 0xff);
    builder.write(0, V2_FREQ_HI, 0xff);
    builder.write(0, V2_CTRL, 0x08);
    builder.write(0, V3_FREQ_LO, 0xff);
    builder.write(0, V3_FREQ_HI, 0xff);
    builder.write(0, V3_CTRL, 0x08);
    builder.write(1, V2_CTRL, 0x20);
    builder.write(1, V3_CTRL, 0x00);
    builder.observe(2, "waveform_zero", fields);
    builder.write(2, V3_CTRL, 0x10);
    builder.observe(3, "triangle_lower", fields);
    builder.write(3, V3_CTRL, 0x14);
    builder.observe(4, "ring_mod_source_clear", fields);
    builder.write(4, V3_CTRL, 0x10);
    builder.observe(132, "triangle_upper", fields);
    builder.write(132, V3_CTRL, 0x14);
    builder.observe(133, "ring_mod_source_high", fields);
    builder.write(133, V3_CTRL, 0x20);
    builder.observe(134, "saw", fields);
    builder.write(134, V3_PW_LO, 0x01);
    builder.write(134, V3_PW_HI, 0x00);
    builder.write(134, V3_CTRL, 0x40);
    builder.observe(135, "pulse_above_width", fields);
    builder.write(135, V3_CTRL, 0x48);
    builder.observe(135, "pulse_test_forced_high", fields);
    builder.write(136, V3_CTRL, 0x80);
    builder.observe(140, "noise", fields);
    builder.write(140, V3_CTRL, 0x30);
    builder.observe(141, "combined_triangle_saw", fields);
    return builder.finish();
}

Case oscillator_pulse_boundary_case() {
    CaseBuilder builder(
        "oscillator_pulse_boundary",
        "Pulse output immediately below, at, and above its programmed width.",
        reSIDfp::CSG8580
    );
    const std::vector<std::string> fields{"public.osc3", "voices.3.accumulator"};
    builder.write(0, V3_FREQ_LO, 0x01);
    builder.write(0, V3_FREQ_HI, 0x00);
    builder.write(0, V3_PW_LO, 0x01);
    builder.write(0, V3_PW_HI, 0x00);
    builder.write(0, V3_CTRL, 0x48);
    builder.write(1, V3_CTRL, 0x40);
    builder.observe(4096, "below_width", fields);
    builder.observe(4097, "at_width", fields);
    builder.observe(4098, "above_width", fields);
    builder.write(4098, V3_CTRL, 0x48);
    builder.observe(4098, "test_forces_high", fields);
    return builder.finish();
}

unsigned int voice_base(const unsigned int voice) { return voice * 7; }

Case sync_route_case(const unsigned int destination) {
    const unsigned int source = (destination + 2) % 3;
    CaseBuilder builder(
        "sync_voice_" + std::to_string(destination + 1) + "_from_voice_" + std::to_string(source + 1),
        "Hard-sync route at the source accumulator MSB rising edge.",
        reSIDfp::CSG8580
    );
    const auto dest_base = voice_base(destination);
    const auto source_base = voice_base(source);
    builder.write(0, dest_base, 0x01);
    builder.write(0, dest_base + 1, 0x00);
    builder.write(0, source_base, 0xff);
    builder.write(0, source_base + 1, 0xff);
    builder.write(0, dest_base + 4, 0x0a);
    builder.write(0, source_base + 4, 0x08);
    builder.write(1, dest_base + 4, 0x02);
    builder.write(1, source_base + 4, 0x20);
    const std::vector<std::string> fields{
        "voices." + std::to_string(destination + 1) + ".accumulator",
        "voices." + std::to_string(source + 1) + ".accumulator"
    };
    builder.observe(129, "before_source_edge", fields);
    builder.observe(130, "source_edge", fields);
    builder.observe(131, "after_source_edge", fields);
    return builder.finish();
}

Case sync_simultaneous_case() {
    CaseBuilder builder(
        "sync_simultaneous_edges",
        "All sources cross the MSB together, exercising source-reset suppression.",
        reSIDfp::CSG8580
    );
    for (unsigned int voice = 0; voice < 3; ++voice) {
        const auto base = voice_base(voice);
        builder.write(0, base, 0xff);
        builder.write(0, base + 1, 0xff);
        builder.write(0, base + 4, 0x0a);
    }
    for (unsigned int voice = 0; voice < 3; ++voice) {
        const auto base = voice_base(voice);
        builder.write(1, base + 4, 0x22);
    }
    const std::vector<std::string> fields{
        "voices.1.accumulator", "voices.2.accumulator", "voices.3.accumulator"
    };
    builder.observe(129, "before_edges", fields);
    builder.observe(130, "simultaneous_edges", fields);
    builder.observe(131, "after_edges", fields);
    return builder.finish();
}

Case sync_toggle_case() {
    CaseBuilder builder(
        "sync_enable_boundary",
        "Sync disabled immediately before one source edge and enabled immediately before the next.",
        reSIDfp::CSG8580
    );
    builder.write(0, V1_FREQ_LO, 0x01);
    builder.write(0, V1_FREQ_HI, 0x00);
    builder.write(0, V1_CTRL, 0x08);
    builder.write(0, V3_FREQ_LO, 0xff);
    builder.write(0, V3_FREQ_HI, 0xff);
    builder.write(0, V3_CTRL, 0x08);
    builder.write(1, V1_CTRL, 0x00);
    builder.write(1, V3_CTRL, 0x20);
    builder.observe(129, "before_disabled_edge", {"voices.1.accumulator"});
    builder.observe(130, "disabled_at_edge", {"voices.1.accumulator"});
    builder.write(385, V1_CTRL, 0x02);
    builder.observe(385, "enabled_before_edge", {"voices.1.accumulator"});
    builder.observe(386, "enabled_edge", {"voices.1.accumulator"});
    return builder.finish();
}

Case sync_noise_order_case() {
    CaseBuilder builder(
        "sync_noise_same_cycle",
        "The destination noise LFSR clocks from its natural bit-19 rise before hard sync resets it.",
        reSIDfp::CSG8580
    );
    builder.write(0, V2_FREQ_LO, 0xe1);
    builder.write(0, V2_FREQ_HI, 0x0f);
    builder.write(0, V2_CTRL, 0x8a);
    builder.write(0, V1_FREQ_LO, 0xff);
    builder.write(0, V1_FREQ_HI, 0xff);
    builder.write(0, V1_CTRL, 0x08);
    builder.write(1, V2_CTRL, 0x82);
    builder.write(1, V1_CTRL, 0x20);
    const std::vector<std::string> fields{
        "voices.2.accumulator", "voices.2.noise_clock_count", "voices.1.accumulator"
    };
    builder.observe(129, "before_joint_edge", fields);
    builder.observe(130, "joint_edge", fields);
    builder.observe(131, "after_joint_edge", fields);
    return builder.finish();
}

Case test_noise_case(const reSIDfp::ChipModel model) {
    CaseBuilder builder(
        std::string("test_noise_") + model_name(model),
        "TEST set, held, rewritten, filled, cleared, and followed by pure-noise clocks.",
        model
    );
    const std::vector<std::string> fields{
        "public.osc3", "voices.3.accumulator", "voices.3.shift_register", "voices.3.test_fill_at"
    };
    builder.write(0, V3_FREQ_LO, 0xff);
    builder.write(0, V3_FREQ_HI, 0xff);
    builder.write(0, V3_CTRL, 0x88);
    builder.observe(0, "test_set", fields);
    builder.observe(10, "test_held_short", fields);
    builder.write(10, V3_CTRL, 0x88);
    builder.observe(10, "test_rewritten", fields);
    const auto remaining_fill_cycles = builder.state().shift_register_reset[2];
    if (remaining_fill_cycles == 0) {
        throw std::runtime_error("TEST rewrite unexpectedly has no pending fill boundary");
    }
    const std::uint64_t fill_boundary = builder.current_cycle() + remaining_fill_cycles;
    builder.observe(fill_boundary - 1, "before_fill", fields);
    builder.observe(fill_boundary, "fill_boundary", fields);
    builder.observe(fill_boundary + 1, "after_fill", fields);
    builder.write(fill_boundary + 1, V3_CTRL, 0x80);
    builder.observe(fill_boundary + 1, "test_clear", fields);
    builder.observe(fill_boundary + 2, "test_clear_pipeline_1", fields);
    builder.observe(fill_boundary + 3, "test_clear_pipeline_2", fields);
    for (unsigned int index = 0; index < 8; ++index) {
        builder.observe(
            fill_boundary + 20 + index * 16,
            "noise_step_" + std::to_string(index),
            fields
        );
    }
    builder.observe(fill_boundary + 100000, "noise_large_delta", fields);
    return builder.finish();
}

Case combined_noise_case(const reSIDfp::ChipModel model) {
    CaseBuilder builder(
        std::string("combined_noise_") + model_name(model),
        "Destructive noise writeback under combined waveforms and return to pure noise.",
        model
    );
    const std::vector<std::string> fields{
        "public.osc3", "voices.3.shift_register", "voices.3.noise_poisoned"
    };
    builder.write(0, V3_FREQ_LO, 0xff);
    builder.write(0, V3_FREQ_HI, 0xff);
    builder.write(0, V3_CTRL, 0x88);
    builder.write(1, V3_CTRL, 0x80);
    builder.observe(40, "pure_noise", fields);
    builder.write(40, V3_CTRL, 0x90);
    builder.observe(80, "noise_triangle", fields);
    builder.write(80, V3_CTRL, 0xa0);
    builder.observe(120, "noise_saw", fields);
    builder.write(120, V3_CTRL, 0xc0);
    builder.observe(160, "noise_pulse", fields);
    builder.write(160, V3_CTRL, 0x80);
    builder.observe(200, "pure_noise_after_writeback", fields);
    return builder.finish();
}

Case captured_reads_case() {
    CaseBuilder builder(
        "live_read_test_sid_window",
        "Ordered OSC3 and ENV3 reads captured by captured_read_fixture_matches_live_read_psid_capture.",
        reSIDfp::CSG8580
    );
    builder.write(2, V3_FREQ_LO, 0xff);
    builder.write(8, V3_FREQ_HI, 0x20);
    builder.write(14, V3_AD, 0x00);
    builder.write(20, V3_SR, 0xf0);
    builder.write(26, V3_CTRL, 0x21);
    builder.observe(19656, "first_osc3_read", {"public.osc3"});
    builder.observe(19660, "first_env3_read", {"public.env3"});
    builder.observe(39312, "second_osc3_read", {"public.osc3"});
    builder.observe(39316, "second_env3_read", {"public.env3"});
    return builder.finish();
}

void write_fields(std::ostream& output, const JsonFields& fields, const std::string& indent) {
    std::size_t index = 0;
    for (const auto& [name, value] : fields) {
        output << indent << quote(name) << ": " << value;
        output << (++index == fields.size() ? "\n" : ",\n");
    }
}

void write_operation(std::ostream& output, const Operation& operation, const bool last) {
    output << "        {\n";
    output << "          \"kind\": "
           << quote(operation.kind == Operation::Kind::Write ? "write" : "observe") << ",\n";
    output << "          \"sequence\": " << operation.sequence << ",\n";
    output << "          \"cycle\": " << operation.cycle;
    if (operation.kind == Operation::Kind::Write) {
        output << ",\n          \"register\": " << operation.reg;
        output << ",\n          \"value\": " << operation.value << '\n';
    } else {
        output << ",\n          \"observation\": " << quote(operation.observation) << '\n';
    }
    output << "        }" << (last ? "\n" : ",\n");
}

void write_observation(std::ostream& output, const Observation& observation, const bool last) {
    output << "        {\n";
    output << "          \"id\": " << quote(observation.id) << ",\n";
    output << "          \"cycle\": " << observation.cycle << ",\n";
    output << "          \"values\": {\n";
    write_fields(output, observation.values, "            ");
    output << "          }\n";
    output << "        }" << (last ? "\n" : ",\n");
}

void write_case(std::ostream& output, const Case& test_case, const bool last) {
    output << "    {\n";
    output << "      \"id\": " << quote(test_case.id) << ",\n";
    output << "      \"description\": " << quote(test_case.description) << ",\n";
    output << "      \"sid_model\": " << quote(model_name(test_case.model)) << ",\n";
    output << "      \"operations\": [\n";
    for (std::size_t index = 0; index < test_case.operations.size(); ++index) {
        write_operation(output, test_case.operations[index], index + 1 == test_case.operations.size());
    }
    output << "      ],\n";
    output << "      \"observations\": [\n";
    for (std::size_t index = 0; index < test_case.observations.size(); ++index) {
        write_observation(output, test_case.observations[index], index + 1 == test_case.observations.size());
    }
    output << "      ]\n";
    output << "    }" << (last ? "\n" : ",\n");
}

void write_document(const std::filesystem::path& path, const std::vector<Case>& cases) {
    std::ofstream output(path, std::ios::binary | std::ios::trunc);
    if (!output) {
        throw std::runtime_error("failed to open oracle output " + path.string());
    }
    output << "{\n";
    output << "  \"schema_version\": 1,\n";
    output << "  \"source\": {\n";
    output << "    \"engine\": \"libresidfp\",\n";
    output << "    \"repository_url\": " << quote(ORACLE_SOURCE_REPOSITORY) << ",\n";
    output << "    \"version\": " << quote(ORACLE_SOURCE_VERSION) << ",\n";
    output << "    \"release_tag\": " << quote(ORACLE_SOURCE_TAG) << ",\n";
    output << "    \"revision\": " << quote(ORACLE_SOURCE_REVISION) << ",\n";
    output << "    \"source_archive_sha256\": " << quote(ORACLE_SOURCE_SHA256) << ",\n";
    output << "    \"generator_revision\": " << quote(ORACLE_GENERATOR_REVISION) << ",\n";
    output << "    \"compiler\": " << quote(ORACLE_COMPILER_ID) << ",\n";
    output << "    \"build_flags\": " << quote(ORACLE_BUILD_FLAGS) << ",\n";
    output << "    \"combined_waveforms\": \"average\"\n";
    output << "  },\n";
    output << "  \"cases\": [\n";
    for (std::size_t index = 0; index < cases.size(); ++index) {
        write_case(output, cases[index], index + 1 == cases.size());
    }
    output << "  ]\n";
    output << "}\n";
}

void write_sorted_document(
    const std::filesystem::path& path,
    std::vector<Case> cases
) {
    std::sort(cases.begin(), cases.end(), [](const Case& left, const Case& right) {
        return left.id < right.id;
    });
    write_document(path, cases);
}

} // namespace

int main(int argc, char** argv) {
    if (argc != 2) {
        return 2;
    }
    try {
        const std::filesystem::path output_directory(argv[1]);
        std::filesystem::create_directories(output_directory);
        std::cerr << "generating seed\n";
        write_sorted_document(output_directory / "seed.oracle.json", {
            reset_case(reSIDfp::MOS6581), reset_case(reSIDfp::CSG8580)
        });
        std::vector<Case> envelope_cases;
        std::cerr << "generating envelope\n";
        for (unsigned int attack = 0; attack < 16; ++attack) {
            std::cerr << "  attack " << attack << '\n';
            envelope_cases.push_back(envelope_attack_rate_case(attack));
        }
        envelope_cases.push_back(envelope_adsr_delay_case());
        envelope_cases.push_back(envelope_same_cycle_order_case(false));
        envelope_cases.push_back(envelope_same_cycle_order_case(true));
        envelope_cases.push_back(envelope_attack_to_release_case());
        std::cerr << "  transitions\n";
        envelope_cases.push_back(envelope_transitions_case());
        write_sorted_document(output_directory / "envelope.oracle.json", std::move(envelope_cases));
        std::cerr << "generating oscillator\n";
        write_sorted_document(output_directory / "oscillator.oracle.json", {
            oscillator_pulse_boundary_case(), oscillator_trajectory_case(), oscillator_waveforms_case()
        });
        std::cerr << "generating sync\n";
        write_sorted_document(output_directory / "sync.oracle.json", {
            sync_noise_order_case(), sync_route_case(0), sync_route_case(1), sync_route_case(2),
            sync_simultaneous_case(), sync_toggle_case()
        });
        std::cerr << "generating noise 6581\n";
        write_sorted_document(output_directory / "test_noise_6581.oracle.json", {
            combined_noise_case(reSIDfp::MOS6581), test_noise_case(reSIDfp::MOS6581)
        });
        std::cerr << "generating noise 8580\n";
        write_sorted_document(output_directory / "test_noise_8580.oracle.json", {
            combined_noise_case(reSIDfp::CSG8580), test_noise_case(reSIDfp::CSG8580)
        });
        std::cerr << "generating captured reads\n";
        write_sorted_document(output_directory / "captured_reads.oracle.json", {
            captured_reads_case()
        });
        std::ofstream files(output_directory / "generated-files.txt", std::ios::trunc);
        files << "captured_reads.oracle.json\n";
        files << "envelope.oracle.json\n";
        files << "oscillator.oracle.json\n";
        files << "seed.oracle.json\n";
        files << "sync.oracle.json\n";
        files << "test_noise_6581.oracle.json\n";
        files << "test_noise_8580.oracle.json\n";
        return files ? 0 : 1;
    } catch (const std::exception& error) {
        std::cerr << "digital SID oracle generation failed: " << error.what() << '\n';
        return 1;
    }
}
