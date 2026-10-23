#pragma once
#include <cstddef>
#include <string_view>

// The profile loader must establish that each reasoning delimiter is one token.
// This state belongs to a single request; neither delimiters nor hidden pieces
// are eligible for delivery, but both consume the generation budget.
struct ReasoningFilter {
    enum class Kind { visible, hidden, delimiter };
    bool in_reasoning = false;
    std::size_t visible_tokens = 0;
    std::size_t hidden_tokens = 0;
    Kind accept(std::string_view piece) {
        if (piece == "<think>" || piece == "</think>") {
            in_reasoning = piece == "<think>";
            ++hidden_tokens;
            return Kind::delimiter;
        }
        if (in_reasoning) {
            ++hidden_tokens;
            return Kind::hidden;
        }
        ++visible_tokens;
        return Kind::visible;
    }
};
