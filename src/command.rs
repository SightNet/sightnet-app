use std::str::FromStr;

#[derive(Debug)]
pub struct Command {
    pub(crate) command_type: CommandType,
    pub(crate) args: Vec<String>,
}

impl FromStr for Command {
    type Err = ();

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let tokens = input.split(' ').collect::<Vec<&str>>();

        if tokens.len() < 2 {
            return Err(());
        }

        let command_type = CommandType::from_str(tokens[0])?;
        let args = tokens[1..tokens.len()]
            .iter()
            .map(|x| x.to_string())
            .collect();

        Ok(Self { command_type, args })
    }
}

impl ToString for Command {
    fn to_string(&self) -> String {
        format!("{} {}", self.command_type.to_string(), self.args.join(" "))
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum CommandType {
    Search,
}

impl FromStr for CommandType {
    type Err = ();

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        match input {
            "search" => Ok(CommandType::Search),
            _ => Err(()),
        }
    }
}

impl ToString for CommandType {
    fn to_string(&self) -> String {
        match self {
            CommandType::Search => "search".to_string(),
        }
    }
}
