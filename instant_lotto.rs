use anchor_lang::prelude::*;
use anchor_spl::token::{self, Mint, Token, TokenAccount};
use anchor_lang::solana_program::hash::hash;

declare_id!("Apsj9Xp8EEpAoZLv5tzgpFa2B9wCeCTmVmR8UiQvieQx");

#[program]
pub mod instant_lottery {
    use super::*;

    pub fn initialize(ctx: Context<Initialize>, min_bet: u64) -> Result<()> {
        let lottery = &mut ctx.accounts.lottery;
        lottery.authority = ctx.accounts.authority.key();
        lottery.token_mint = ctx.accounts.token_mint.key();
        lottery.min_bet = min_bet;
        lottery.locked = false;
        lottery.total_weight = 10000;
        lottery.weight_ranges = [4660, 7627, 8858, 9578, 10000];
        lottery.multipliers = [2, 5, 10, 50, 100];
        lottery.pool_amount = 0;
        lottery.play_times = 0;
        lottery.prize_amount = 0;
        Ok(())
    }

    pub fn play(ctx: Context<Play>, amount: u64, uuid: String) -> Result<()> {
        let lottery = &mut ctx.accounts.lottery;
        require!(!lottery.locked, LotteryError::LotteryLocked);
        require!(amount >= lottery.min_bet, LotteryError::BetTooSmall);

        token::transfer(
            CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                token::Transfer {
                    from: ctx.accounts.player_token.to_account_info(),
                    to: ctx.accounts.token_account.to_account_info(),
                    authority: ctx.accounts.player.to_account_info(),
                },
            ),
            amount,
        )?;

        lottery.pool_amount = lottery
            .pool_amount
            .checked_add(amount)
            .ok_or(LotteryError::ArithmeticOverflow)?;

        lottery.play_times = lottery
            .play_times
            .checked_add(1)
            .ok_or(LotteryError::ArithmeticOverflow)?;

        let mut random_seed = ctx.accounts.recent_blockhashes.key().to_bytes().to_vec();
        random_seed.extend_from_slice(&ctx.accounts.player.key().to_bytes());
        random_seed.extend_from_slice(&Clock::get()?.slot.to_le_bytes());
        random_seed.extend_from_slice(&Clock::get()?.unix_timestamp.to_le_bytes());
        random_seed.extend_from_slice(uuid.as_bytes());

        let hash = hash(&random_seed);
        let hash_bytes = hash.to_bytes();

        let numbers: [u8; 3] = (0..3)
            .map(|i| {
                let slice = &hash_bytes[i * 8..(i + 1) * 8];
                let random =
                    u64::from_le_bytes(slice.try_into().unwrap()) % lottery.total_weight as u64;
                get_number(random, &lottery.weight_ranges)
            }).collect::<Vec<_>>()
            .try_into()
            .unwrap();

        let win_multiplier = if numbers.windows(2).all(|w| w[0] == w[1]) {
            lottery.multipliers[(numbers[0] - 1) as usize]
        } else {
            0
        };

        if win_multiplier > 0 {
            let total_prize = amount
                .checked_mul(win_multiplier as u64)
                .and_then(|x| x.checked_mul(101))
                .and_then(|x| x.checked_div(100))
                .ok_or(LotteryError::ArithmeticOverflow)?;

            lottery.prize_amount = lottery
                .prize_amount
                .checked_add(total_prize)
                .ok_or(LotteryError::ArithmeticOverflow)?;
        }

        emit!(PlayEvent {
            player: ctx.accounts.player.key(),
            amount,
            numbers,
            win_multiplier,
        });

        Ok(())
    }

    pub fn claim_prize(
        ctx: Context<ClaimPrize>,
        prize_amount: u64,
        fee_amount: u64,
        timestamp: i64,
    ) -> Result<()> {
        require!(
            ctx.accounts.lottery.prize_amount > 0,
            LotteryError::InsufficientPrize
        );

        let clock = Clock::get()?;
        require!(
            clock.unix_timestamp - timestamp < 300,
            LotteryError::SignatureExpired
        );

        let total_amount = prize_amount
            .checked_add(fee_amount)
            .ok_or(LotteryError::ArithmeticOverflow)?;

        require!(
            ctx.accounts.lottery.prize_amount >= total_amount,
            LotteryError::InsufficientPrize
        );

        let auth_key = ctx.accounts.lottery.authority;
        let authority_ref = auth_key.as_ref();
        let signer_seeds = &[b"lottery" as &[u8], authority_ref, &[ctx.bumps.lottery]];

        token::transfer(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                token::Transfer {
                    from: ctx.accounts.token_account.to_account_info(),
                    to: ctx.accounts.player_token.to_account_info(),
                    authority: ctx.accounts.lottery.to_account_info(),
                },
                &[signer_seeds],
            ),
            prize_amount,
        )?;

        token::transfer(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                token::Transfer {
                    from: ctx.accounts.token_account.to_account_info(),
                    to: ctx.accounts.dev_token.to_account_info(),
                    authority: ctx.accounts.lottery.to_account_info(),
                },
                &[signer_seeds],
            ),
            fee_amount,
        )?;

        let lottery = &mut ctx.accounts.lottery;
        lottery.pool_amount = lottery
            .pool_amount
            .checked_sub(prize_amount)
            .and_then(|amount| amount.checked_sub(fee_amount))
            .ok_or(LotteryError::ArithmeticOverflow)?;

        lottery.prize_amount = lottery
            .prize_amount
            .checked_sub(prize_amount)
            .and_then(|amount| amount.checked_sub(fee_amount))
            .ok_or(LotteryError::ArithmeticOverflow)?;

        emit!(ClaimEvent {
            player: ctx.accounts.player.key(),
            actual_prize: prize_amount,
            actual_fee: fee_amount,
        });

        Ok(())
    }

    pub fn set_locked(
        ctx: Context<AdminAction>, 
        locked: Option<bool>,
        total_weight: Option<u32>,
        weight_ranges: Option<[u32; 5]>,
        multipliers: Option<[u8; 5]>,
    ) -> Result<()> {
        let lottery = &mut ctx.accounts.lottery;
        
        if let Some(lock_status) = locked {
            lottery.locked = lock_status;
            
            emit!(PauseStatusEvent {
                locked: lock_status,
                timestamp: Clock::get()?.unix_timestamp,
            });
        }
        
        if let Some(weight) = total_weight {
            lottery.total_weight = weight;
        }
        
        if let Some(ranges) = weight_ranges {
            lottery.weight_ranges = ranges;
        }
        
        if let Some(mults) = multipliers {
            lottery.multipliers = mults;
        }

        Ok(())
    }
}

#[derive(Accounts)]
#[instruction(min_bet: u64)]
pub struct Initialize<'info> {
    #[account(
        init,
        payer = authority,
        space = 8 + 32 + 32 + 8 + 1 + 4 + 20 + 5 + 8 + 8 + 8, 
        seeds = [b"lottery", authority.key().as_ref()],
        bump
    )]
    pub lottery: Account<'info, Lottery>,

    pub token_mint: Account<'info, Mint>,

    #[account(
        init,
        payer = authority,
        seeds = [b"token_account", authority.key().as_ref()],
        bump,
        token::mint = token_mint,
        token::authority = lottery,
    )]
    pub token_account: Box<Account<'info, TokenAccount>>,

    #[account(mut)]
    pub authority: Signer<'info>,

    pub system_program: Program<'info, System>,
    pub token_program: Program<'info, Token>,
    pub rent: Sysvar<'info, Rent>,
}

#[derive(Accounts)]
pub struct Play<'info> {
    #[account(
        mut,
        seeds = [b"lottery", lottery.authority.as_ref()],
        bump
    )]
    pub lottery: Account<'info, Lottery>,

    #[account(
        mut,
        seeds = [b"token_account", lottery.authority.as_ref()],
        bump,
    )]
    pub token_account: Account<'info, TokenAccount>,

    #[account(
        mut,
        constraint = player_token.mint == lottery.token_mint,
        constraint = player_token.owner == player.key()
    )]
    pub player_token: Account<'info, TokenAccount>,

    #[account(mut)]
    pub player: Signer<'info>,

    /// CHECK: Recent blockhashes is used for randomness
    pub recent_blockhashes: AccountInfo<'info>,

    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct ClaimPrize<'info> {
    #[account(
        mut,
        seeds = [b"lottery", lottery.authority.as_ref()],
        bump
    )]
    pub lottery: Account<'info, Lottery>,

    #[account(
        signer, 
        constraint = authority.key() == lottery.authority @ LotteryError::InvalidAuthority
    )]
    /// CHECK: Authority signer
    pub authority: AccountInfo<'info>,

    #[account(
        mut,
        seeds = [b"token_account", lottery.authority.as_ref()],
        bump
    )]
    pub token_account: Account<'info, TokenAccount>,

    #[account(
        mut,
        constraint = player_token.mint == lottery.token_mint,
        constraint = player_token.owner == player.key()
    )]
    pub player_token: Account<'info, TokenAccount>,

    #[account(
        mut,
        constraint = dev_token.mint == lottery.token_mint
    )]
    pub dev_token: Account<'info, TokenAccount>,

    #[account(signer)]
    pub player: Signer<'info>,
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct AdminAction<'info> {
    #[account(
        mut,
        seeds = [b"lottery", lottery.authority.as_ref()],
        bump,
        constraint = lottery.authority == authority.key()
    )]
    pub lottery: Account<'info, Lottery>,
    pub authority: Signer<'info>,
}

#[account]
#[derive(Default, PartialEq)]
pub struct Lottery {
    pub authority: Pubkey,
    pub token_mint: Pubkey,
    pub min_bet: u64,
    pub locked: bool,
    pub total_weight: u32,
    pub weight_ranges: [u32; 5],
    pub multipliers: [u8; 5],
    pub pool_amount: u64,
    pub play_times: u64,
    pub prize_amount: u64,
}

#[derive(AnchorSerialize, AnchorDeserialize)]
pub struct ClaimMessage {
    pub player: Pubkey,
    pub prize_amount: u64,
    pub fee_amount: u64,
    pub nonce: [u8; 8],
    pub timestamp: i64,
}

#[error_code]
pub enum LotteryError {
    #[msg("Lottery is locked")]
    LotteryLocked,
    #[msg("Bet amount is too small")]
    BetTooSmall,
    #[msg("Invalid authority signature")]
    InvalidSignature,
    #[msg("Arithmetic overflow occurred")]
    ArithmeticOverflow,
    #[msg("Signature has expired")]
    SignatureExpired,
    #[msg("Invalid authority")]
    InvalidAuthority,
    #[msg("Insufficient prize amount available")]
    InsufficientPrize,
}

#[event]
pub struct PlayEvent {
    pub player: Pubkey,
    pub amount: u64,
    pub numbers: [u8; 3],
    pub win_multiplier: u8,
}

#[event]
pub struct PauseStatusEvent {
    pub locked: bool,
    pub timestamp: i64,
}

#[event]
pub struct ClaimEvent {
    pub player: Pubkey,
    pub actual_prize: u64,
    pub actual_fee: u64,
}

fn get_number(random: u64, weight_ranges: &[u32; 5]) -> u8 {
    for (i, &range) in weight_ranges.iter().enumerate() {
        if random < range as u64 {
            return (i + 1) as u8;
        }
    }
    5
}
