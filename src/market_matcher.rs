use tracing::{debug, warn};

use crate::signals::{SignalDirection, TechnicalSignal};
use crate::types::{Direction, Market};

/// Type of prediction market question
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MarketType {
    UpQuestion,     // "Will X be UP?"
    DownQuestion,   // "Will X be DOWN?"
    Unknown,
}

/// Parse market question to determine type
pub fn parse_market_question(question: &str) -> MarketType {
    let q = question.to_lowercase();

    // UP/Higher/Above patterns
    if q.contains("up") || q.contains("higher") || q.contains("above") || q.contains("increase") {
        return MarketType::UpQuestion;
    }

    // DOWN/Lower/Below patterns
    if q.contains("down") || q.contains("lower") || q.contains("below") || q.contains("decrease") {
        return MarketType::DownQuestion;
    }

    // Could add more complex parsing for price range questions
    // e.g., "Will BTC be between $X and $Y?"
    // For now, return Unknown

    MarketType::Unknown
}

/// Match technical signal to market question and determine YES/NO direction
///
/// Logic:
/// - If signal says UP and market asks "Will X be UP?" → BUY YES
/// - If signal says UP and market asks "Will X be DOWN?" → BUY NO
/// - If signal says DOWN and market asks "Will X be DOWN?" → BUY YES
/// - If signal says DOWN and market asks "Will X be UP?" → BUY NO
pub fn match_signal_to_market(
    signal: &TechnicalSignal,
    market: &Market,
) -> Option<Direction> {
    let market_type = parse_market_question(&market.question);

    debug!(
        question = %market.question,
        market_type = ?market_type,
        signal_direction = ?signal.direction,
        "matching signal to market"
    );

    match (signal.direction, market_type) {
        // Signal UP, Market asks UP → YES
        (SignalDirection::Up, MarketType::UpQuestion) => {
            debug!("Signal UP + UP question → BUY YES");
            Some(Direction::Yes)
        }

        // Signal UP, Market asks DOWN → NO
        (SignalDirection::Up, MarketType::DownQuestion) => {
            debug!("Signal UP + DOWN question → BUY NO");
            Some(Direction::No)
        }

        // Signal DOWN, Market asks DOWN → YES
        (SignalDirection::Down, MarketType::DownQuestion) => {
            debug!("Signal DOWN + DOWN question → BUY YES");
            Some(Direction::Yes)
        }

        // Signal DOWN, Market asks UP → NO
        (SignalDirection::Down, MarketType::UpQuestion) => {
            debug!("Signal DOWN + UP question → BUY NO");
            Some(Direction::No)
        }

        // Unknown market type
        (_, MarketType::Unknown) => {
            warn!(
                question = %market.question,
                "could not parse market question type"
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn test_parse_up_questions() {
        assert_eq!(
            parse_market_question("Will BTC be UP in the next 5 minutes?"),
            MarketType::UpQuestion
        );
        assert_eq!(
            parse_market_question("Will ETH close higher than $3000?"),
            MarketType::UpQuestion
        );
        assert_eq!(
            parse_market_question("Will price increase?"),
            MarketType::UpQuestion
        );
    }

    #[test]
    fn test_parse_down_questions() {
        assert_eq!(
            parse_market_question("Will BTC be DOWN in the next 5 minutes?"),
            MarketType::DownQuestion
        );
        assert_eq!(
            parse_market_question("Will ETH close lower than $3000?"),
            MarketType::DownQuestion
        );
        assert_eq!(
            parse_market_question("Will price decrease?"),
            MarketType::DownQuestion
        );
    }

    #[test]
    fn test_parse_unknown_questions() {
        assert_eq!(
            parse_market_question("Will BTC be between $50k and $60k?"),
            MarketType::Unknown
        );
    }

    fn mock_market(question: &str) -> Market {
        Market {
            condition_id: "test".to_string(),
            question: question.to_string(),
            asset: "btc".to_string(),
            duration: "5m".to_string(),
            yes_price: dec!(0.5),
            no_price: dec!(0.5),
            end_date_ms: 0,
            liquidity: dec!(1000),
        }
    }

    fn mock_signal(direction: SignalDirection) -> TechnicalSignal {
        TechnicalSignal {
            direction,
            probability: dec!(0.7),
            confidence: dec!(0.8),
            reasoning: "test".to_string(),
        }
    }

    #[test]
    fn test_signal_up_market_up() {
        let signal = mock_signal(SignalDirection::Up);
        let market = mock_market("Will BTC be UP in 5m?");

        let result = match_signal_to_market(&signal, &market);
        assert_eq!(result, Some(Direction::Yes));
    }

    #[test]
    fn test_signal_up_market_down() {
        let signal = mock_signal(SignalDirection::Up);
        let market = mock_market("Will BTC be DOWN in 5m?");

        let result = match_signal_to_market(&signal, &market);
        assert_eq!(result, Some(Direction::No));
    }

    #[test]
    fn test_signal_down_market_down() {
        let signal = mock_signal(SignalDirection::Down);
        let market = mock_market("Will BTC be DOWN in 5m?");

        let result = match_signal_to_market(&signal, &market);
        assert_eq!(result, Some(Direction::Yes));
    }

    #[test]
    fn test_signal_down_market_up() {
        let signal = mock_signal(SignalDirection::Down);
        let market = mock_market("Will BTC be UP in 5m?");

        let result = match_signal_to_market(&signal, &market);
        assert_eq!(result, Some(Direction::No));
    }

    #[test]
    fn test_unknown_market_type() {
        let signal = mock_signal(SignalDirection::Up);
        let market = mock_market("Will BTC be between $50k and $60k?");

        let result = match_signal_to_market(&signal, &market);
        assert_eq!(result, None);
    }

    #[test]
    fn test_parse_higher_keyword() {
        let market = mock_market("Will BTC be HIGHER in 5 minutes?");
        let market_type = parse_market_question(&market.question);
        assert_eq!(market_type, MarketType::UpQuestion);
    }

    #[test]
    fn test_parse_lower_keyword() {
        let market = mock_market("Will ETH be LOWER after 5m?");
        let market_type = parse_market_question(&market.question);
        assert_eq!(market_type, MarketType::DownQuestion);
    }

    #[test]
    fn test_parse_case_insensitive() {
        let market1 = mock_market("Will BTC be UP?");
        let market2 = mock_market("Will BTC be up?");
        let market3 = mock_market("Will BTC be Up?");

        assert_eq!(parse_market_question(&market1.question), MarketType::UpQuestion);
        assert_eq!(parse_market_question(&market2.question), MarketType::UpQuestion);
        assert_eq!(parse_market_question(&market3.question), MarketType::UpQuestion);
    }

    #[test]
    fn test_all_combinations() {
        // Test all 4 valid combinations
        let combos = vec![
            (SignalDirection::Up, "Will BTC be UP?", Some(Direction::Yes)),
            (SignalDirection::Up, "Will BTC be DOWN?", Some(Direction::No)),
            (SignalDirection::Down, "Will BTC be DOWN?", Some(Direction::Yes)),
            (SignalDirection::Down, "Will BTC be UP?", Some(Direction::No)),
        ];

        for (signal_dir, question, expected_dir) in combos {
            let signal = mock_signal(signal_dir);
            let market = mock_market(question);
            let result = match_signal_to_market(&signal, &market);
            assert_eq!(result, expected_dir, "Failed for {:?} + {}", signal_dir, question);
        }
    }

    #[test]
    fn test_ambiguous_question() {
        let signal = mock_signal(SignalDirection::Up);
        let market = mock_market("Will BTC reach a new all-time high?");

        let result = match_signal_to_market(&signal, &market);
        assert_eq!(result, None, "Ambiguous questions should return None");
    }
}
